use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn client(directory: &Path, script: &str) -> AdbClient {
    let executable = directory.join("adb");
    fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: these syscalls have no arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    let mut client = AdbClient::for_uid(uid).unwrap();
    client.executable = executable;
    client.home = directory.into();
    client
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "ADB fixture did not become ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

// Own only the isolated server so a failed assertion cannot leak it.
struct Server {
    child: std::process::Child,
    client: AdbClient,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Reconnection may have forked a replacement server. The isolated
        // socket, rather than just the original child, owns that lifetime.
        let _ = self.client.run(&["kill-server".as_ref()], COMMAND_TIMEOUT);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn host_protocol_accepts_fragmented_replies_and_rejects_invalid_frames() {
    struct Reply {
        input: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
        delay: Duration,
    }
    impl Read for Reply {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            thread::sleep(self.delay);
            let length = buffer.len().min(2);
            self.input.read(&mut buffer[..length])
        }
    }
    impl Write for Reply {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let reply = |input: &[u8]| Reply {
        input: io::Cursor::new(input.to_vec()),
        written: Vec::new(),
        delay: Duration::ZERO,
    };
    let mut version = reply(b"OKAY00040029");
    assert_eq!(
        protocol_request(&mut version, "host:version").unwrap(),
        "0029"
    );
    assert_eq!(version.written, b"000chost:version");
    let error = protocol_request(&mut reply(b"FAIL0004nope"), "host:version").unwrap_err();
    assert_eq!(error.to_string(), "nope");
    for invalid in [&b"NOPE"[..], b"OKAYZZZZ", b"OKAY2001", b"OKAY0004x"] {
        assert!(protocol_request(&mut reply(invalid), "host:version").is_err());
    }
    let mut slow = reply(b"OKAY00040029");
    slow.delay = Duration::from_millis(10);
    let started = Instant::now();
    let error = protocol_request_before(
        slow,
        "host:version",
        started + Duration::from_millis(30),
        |_, _| Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn public_key_rejects_additional_keys_and_malformed_output() {
    let key = format!("{}= user@host\n", "A".repeat(699));
    validate_public_key(&key).unwrap();
    for invalid in [
        String::new(),
        "short".into(),
        key.repeat(2),
        key.replace('A', "_"),
        key.replace('=', "A"),
        format!("{key}\0"),
    ] {
        assert!(validate_public_key(&invalid).is_err());
    }
}

#[test]
fn adb_commands_use_the_owner_home_and_bounded_execution() {
    let directory = tempfile::tempdir().unwrap();
    let client = client(
        directory.path(),
        "case \"$1\" in\n identity) printf '%s\\n' \"$HOME\"; /usr/bin/id -u; /usr/bin/id -g;;\n hang) exec /usr/bin/sleep 30;;\n *) exit 7;;\n esac",
    );
    let identity = client.run(&["identity".as_ref()], COMMAND_TIMEOUT).unwrap();
    assert_eq!(
        identity,
        format!(
            "{}\n{}\n{}\n",
            directory.path().display(),
            client.uid,
            client.gid
        )
    );
    assert!(client.run(&["fail".as_ref()], COMMAND_TIMEOUT).is_err());
    let started = Instant::now();
    let error = client
        .run(&["hang".as_ref()], Duration::from_millis(100))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn retries_failed_connects_and_disconnects_only_its_transport() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let worker_calls = Arc::clone(&calls);
    let ready = Arc::new(AtomicBool::new(false));
    let worker_ready = Arc::clone(&ready);
    let endpoint = "10.177.0.2:5555";
    let connection = AdbConnection::watch(
        endpoint.into(),
        Duration::from_millis(20),
        Duration::from_millis(20),
        move |operation| {
            worker_calls
                .lock()
                .unwrap()
                .push(format!("{operation} {endpoint}"));
            if operation == "connect" && worker_ready.load(Ordering::Relaxed) {
                Ok(format!("connected to {endpoint}"))
            } else {
                Ok("failed to connect".into())
            }
        },
    )
    .unwrap();
    wait_until(|| calls.lock().unwrap().len() >= 2);
    ready.store(true, Ordering::Relaxed);
    wait_until(|| calls.lock().unwrap().len() >= 4);
    drop(connection);
    let calls = calls.lock().unwrap().join("\n");
    assert!(
        calls
            .lines()
            .filter(|line| line.starts_with("connect "))
            .count()
            >= 4
    );
    assert_eq!(calls.lines().last(), Some("disconnect 10.177.0.2:5555"));
    assert!(!calls.contains("kill-server"));
    assert!(!calls.contains("disconnect \n"));
}

#[test]
fn checks_a_connected_transport_only_at_the_connected_interval() {
    let connects = Arc::new(Mutex::new(0_u32));
    let worker_connects = Arc::clone(&connects);
    let endpoint = "10.177.0.2:5555";
    let connection = AdbConnection::watch(
        endpoint.into(),
        Duration::from_millis(5),
        Duration::from_secs(60),
        move |operation| {
            if operation == "connect" {
                *worker_connects.lock().unwrap() += 1;
            }
            Ok(format!("already connected to {endpoint}"))
        },
    )
    .unwrap();
    wait_until(|| *connects.lock().unwrap() >= 1);
    thread::sleep(Duration::from_millis(100));
    drop(connection);
    assert_eq!(*connects.lock().unwrap(), 1);
}

#[test]
fn authentication_overrides_insecure_defaults_and_preserves_other_properties() {
    let original = "# pinned defaults\nro.adb.secure=0\nro.product.device=vsoc_x86_64\nro.product.model=Cuttlefish\nro.build.version.sdk=37\n";
    let properties = authenticated_properties(original);
    assert!(properties.contains("# pinned defaults\n"));
    assert!(properties.contains("ro.build.version.sdk=37\n"));
    assert!(properties.contains("ro.adb.secure=1\n"));
    assert!(properties.contains("ro.product.device=droidloom\n"));
    assert!(properties.contains("ro.product.model=Cuttlefish\n"));
    assert!(!properties.contains("ro.adb.secure=0"));
    assert_eq!(authenticated_properties(&properties), properties);
}

#[test]
fn cleanup_removes_only_generated_configuration_and_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let adb = directory.path().join(DIRECTORY);
    fs::create_dir(&adb).unwrap();
    for name in [KEYS_FILE, "init.rc", "build.prop"] {
        fs::write(adb.join(name), "generated").unwrap();
    }
    fs::write(directory.path().join("unrelated"), "keep").unwrap();
    cleanup(directory.path()).unwrap();
    cleanup(directory.path()).unwrap();
    assert!(!adb.exists());
    assert_eq!(
        fs::read_to_string(directory.path().join("unrelated")).unwrap(),
        "keep"
    );
}

/// A real, isolated ADB server talking to a minimal Android wire-protocol peer.
/// No default ADB server, real device, or user's key is touched.
#[test]
#[ignore = "requires /usr/bin/adb and local socket access"]
fn real_adb_registers_reconnects_selects_by_device_name_and_disconnects() {
    let directory = tempfile::tempdir().unwrap();
    let endpoint = format!(
        "localfilesystem:{}",
        directory.path().join("android.sock").display()
    );
    let listener = UnixListener::bind(directory.path().join("android.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let peer_stop = Arc::clone(&stopped);
    let peer = thread::spawn(move || {
        let mut connections = Vec::new();
        while !peer_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((socket, _)) => {
                    let stop = Arc::clone(&peer_stop);
                    connections.push(thread::spawn(move || android_peer(socket, &stop)));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept Android peer: {error}"),
            }
        }
        for connection in connections {
            connection.join().unwrap();
        }
    });
    let mut client = AdbClient::for_uid(unsafe { libc::geteuid() }).unwrap();
    client.home = directory.path().into();
    client.server_socket = Some(format!(
        "localfilesystem:{}",
        directory.path().join("server.sock").display()
    ));
    let mut server = Server {
        child: client
            .command()
            .args(["server", "nodaemon"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
        client: client.clone(),
    };
    wait_until(|| directory.path().join("server.sock").exists());
    let key = client.public_key().unwrap();
    validate_public_key(&key).unwrap();
    let watcher_client = client.clone();
    let connection =
        AdbConnection::start_at(
            watcher_client,
            endpoint.clone(),
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .unwrap();
    let devices = || client.server_request("host:devices-l").unwrap_or_default();
    wait_until(|| {
        let list = devices();
        list.contains(&endpoint) && list.contains("device:droidloom")
    });
    assert_eq!(
        client
            .run(
                &[
                    "-s".as_ref(),
                    "device:droidloom".as_ref(),
                    "get-state".as_ref()
                ],
                COMMAND_TIMEOUT
            )
            .unwrap()
            .trim(),
        "device"
    );
    // A user can restart their ADB server independently of Android. The
    // watcher must register the same transport with the new server.
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    wait_until(|| devices().contains("device:droidloom"));
    drop(connection);
    assert!(!devices().contains(&endpoint));
    assert!(
        client.server_request("host:version").is_ok(),
        "disconnect must leave the ADB server alive"
    );
    drop(server);
    assert!(
        client.server_request("host:version").is_err(),
        "the fixture must clean up its replacement server"
    );
    stopped.store(true, Ordering::Relaxed);
    peer.join().unwrap();
}

fn android_peer(mut socket: UnixStream, stopped: &AtomicBool) {
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut header = [0_u8; 24];
    if socket.read_exact(&mut header).is_err() {
        return;
    }
    let length = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
    assert!(length < 4096);
    let mut payload = vec![0_u8; length];
    if socket.read_exact(&mut payload).is_err() {
        return;
    }
    let banner = b"device::ro.product.name=droidloom;ro.product.model=Droidloom;ro.product.device=droidloom;\0";
    let command = u32::from_le_bytes(*b"CNXN");
    let mut packet = Vec::new();
    for word in [
        command,
        0x0100_0001,
        1024 * 1024,
        u32::try_from(banner.len()).unwrap(),
        0,
        !command,
    ] {
        packet.extend_from_slice(&word.to_le_bytes());
    }
    packet.extend_from_slice(banner);
    if socket.write_all(&packet).is_err() {
        return;
    }
    while !stopped.load(Ordering::Relaxed) {
        match socket.read(&mut header) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
}
