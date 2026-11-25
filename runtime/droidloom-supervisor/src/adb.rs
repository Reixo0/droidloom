//! Register the private Android transport with the cell owner's ADB server.
//!
//! All host ADB commands run with the owner's credentials and home directory.
//! Only the public key crosses into Android; the private key stays on the host.

use std::ffi::{CStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::CellSpec;

pub(crate) const ENDPOINT: &str = "10.177.0.2:5555";
pub(crate) const DIRECTORY: &str = "adb";
pub(crate) const KEYS_FILE: &str = "keys";
const ADB: &str = "/usr/bin/adb";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// Once connected, the transport is only checked now and then: every check
/// wakes the host and talks to adbd, which kept a phone that had just woken
/// from sleep busy for seconds. The ADB server reconnects a dropped TCP
/// transport by itself; this check covers a restarted server.
const CONNECTED_INTERVAL: Duration = Duration::from_secs(30);
const MAX_OUTPUT: u64 = 8192;

/// Android init owns the non-root adbd service and its restarts.
/// Framework USB state changes must not disable the private TCP transport.
pub(crate) const INIT_POLICY: &str = "
# Droidloom's authenticated, private-network ADB transport.
on post-fs-data
    setprop service.adb.listen_addrs tcp:5555
    start adbd

on property:sys.boot_completed=1
    start adbd

on property:init.svc.adbd=stopped && property:sys.boot_completed=1
    start adbd
";

#[derive(Clone)]
pub(crate) struct AdbClient {
    executable: PathBuf,
    uid: u32,
    gid: u32,
    groups: Vec<libc::gid_t>,
    name: OsString,
    home: PathBuf,
    server_address: SocketAddr,
    #[cfg(test)]
    server_socket: Option<String>,
}

impl AdbClient {
    fn for_uid(uid: u32) -> io::Result<Self> {
        if uid == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "automatic ADB requires a non-root cell owner",
            ));
        }
        let mut buffer = vec![0_u8; 16 * 1024];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers refer to live, appropriately sized storage.
        let error = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut result,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "cell owner has no host account",
            ));
        }
        // SAFETY: getpwuid_r succeeded; its strings remain live in buffer until copied.
        let entry = unsafe { entry.assume_init() };
        let name = unsafe { CStr::from_ptr(entry.pw_name) };
        // Resolve the user's legitimate groups before fork. Keeping these
        // preserves USB-device permissions if ADB starts a fresh server.
        // Inherited daemon/root groups must never cross into that server.
        let mut group_count = 0;
        unsafe {
            libc::getgrouplist(
                name.as_ptr(),
                entry.pw_gid,
                std::ptr::null_mut(),
                &raw mut group_count,
            )
        };
        let count = usize::try_from(group_count)
            .ok()
            .filter(|count| (1..=65_536).contains(count))
            .ok_or_else(|| io::Error::other("invalid cell owner group count"))?;
        let mut groups = vec![0; count];
        let error = unsafe {
            libc::getgrouplist(
                name.as_ptr(),
                entry.pw_gid,
                groups.as_mut_ptr(),
                &raw mut group_count,
            )
        };
        if error < 0 {
            return Err(io::Error::other("could not resolve cell owner groups"));
        }
        groups.truncate(usize::try_from(group_count).expect("validated group count"));
        let name = name.to_bytes().to_vec();
        let home = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes().to_vec();
        Ok(Self {
            executable: ADB.into(),
            uid,
            gid: entry.pw_gid,
            groups,
            name: OsString::from_vec(name),
            home: PathBuf::from(OsString::from_vec(home)),
            server_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 5037)),
            #[cfg(test)]
            server_socket: None,
        })
    }

    fn command(&self) -> Command {
        let command = self.owner_command(&self.executable);
        #[cfg(test)]
        let command = {
            let mut command = command;
            if let Some(socket) = &self.server_socket {
                command.args(["-L", socket]);
            }
            command
        };
        command
    }

    fn owner_command(&self, executable: &Path) -> Command {
        let mut command = droidloom_cpu_placement::command(executable);
        // A system daemon's HOME and ADB settings belong to root, not the cell
        // owner. Use the normal user's default local server and key location.
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("USER", &self.name)
            .env("LOGNAME", &self.name)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let uid = self.uid;
        let gid = self.gid;
        let groups = self.groups.clone();
        // SAFETY: the child uses only async-signal-safe credential syscalls
        // before exec. Only the owner's credentials and groups remain.
        unsafe {
            command.pre_exec(move || {
                if libc::geteuid() == 0 {
                    if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                } else if libc::geteuid() != uid || libc::getegid() != gid {
                    return Err(io::Error::from_raw_os_error(libc::EPERM));
                }
                Ok(())
            });
        }
        command
    }

    fn run(&self, arguments: &[&std::ffi::OsStr], timeout: Duration) -> io::Result<String> {
        let mut command = self.command();
        command.args(arguments);
        run_command(command, timeout)
    }

    fn ensure_server(&self) -> io::Result<()> {
        if self.server_request("host:version").is_ok() {
            return Ok(());
        }
        #[cfg(test)]
        if self.server_socket.is_some() {
            return self
                .run(&["start-server".as_ref()], COMMAND_TIMEOUT)
                .map(|_| ());
        }
        // Spawn via the user's manager: a daemon forked directly by droidloomd
        // stays in its cgroup and systemd kills it when Droidloom stops.
        let mut command = self.owner_command(Path::new("/usr/bin/systemd-run"));
        command
            .env("XDG_RUNTIME_DIR", format!("/run/user/{}", self.uid))
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path=/run/user/{}/bus", self.uid),
            )
            .args([
                "--user",
                "--quiet",
                "--collect",
                "--service-type=forking",
                "--unit=droidloom-adb-server",
                "--property=TimeoutStartSec=5s",
            ]);
        // The user's manager may have imported custom ADB socket/key settings.
        // Match the clean environment used to derive this cell's public key.
        for (name, value) in [
            ("HOME", self.home.as_os_str()),
            ("USER", &self.name),
            ("LOGNAME", &self.name),
        ] {
            let mut setting = OsString::from(format!("--setenv={name}="));
            setting.push(value);
            command.arg(setting);
        }
        // Expand explicit environment assignments exactly once. This preserves
        // literal '$' characters in account paths and works with systemd
        // versions predating --expand-environment=no (v254).
        command.args([
            "/usr/bin/env",
            "-i",
            "PATH=/usr/bin:/bin",
            "HOME=${HOME}",
            "USER=${USER}",
            "LOGNAME=${LOGNAME}",
            ADB,
            "start-server",
        ]);
        run_command(command, COMMAND_TIMEOUT).map(|_| ())
    }

    fn public_key(&self) -> io::Result<String> {
        self.ensure_server()?;
        // Ask ADB to derive the public key as the user. Never read a
        // user-controlled path with the daemon's root filesystem authority.
        let key = self.home.join(".android/adbkey");
        let output = self.run(&["pubkey".as_ref(), key.as_os_str()], COMMAND_TIMEOUT)?;
        validate_public_key(&output)?;
        Ok(output)
    }

    fn server_request(&self, request: &str) -> io::Result<String> {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        #[cfg(test)]
        if let Some(path) = self
            .server_socket
            .as_deref()
            .and_then(|socket| socket.strip_prefix("localfilesystem:"))
        {
            let socket = std::os::unix::net::UnixStream::connect(path)?;
            return protocol_request_before(socket, request, deadline, |socket, remaining| {
                socket.set_read_timeout(Some(remaining))?;
                socket.set_write_timeout(Some(remaining))
            });
        }
        let socket = TcpStream::connect_timeout(&self.server_address, COMMAND_TIMEOUT)?;
        protocol_request_before(socket, request, deadline, |socket, remaining| {
            socket.set_read_timeout(Some(remaining))?;
            socket.set_write_timeout(Some(remaining))
        })
    }

    fn transport_command(&self, operation: &str, endpoint: &str) -> io::Result<String> {
        // Direct host requests never auto-spawn or replace an ADB server,
        // including a server using a different client protocol version.
        self.server_request(&format!("host:{operation}:{endpoint}"))
    }
}

/// Partial reads must share one deadline; a slow stream cannot extend the
/// worker's lifetime by sending each byte just before an individual timeout.
struct DeadlineIo<S> {
    socket: S,
    deadline: Instant,
    set_timeout: fn(&S, Duration) -> io::Result<()>,
}

impl<S> DeadlineIo<S> {
    fn arm_timeout(&self) -> io::Result<()> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "host ADB request timed out"))?;
        (self.set_timeout)(&self.socket, remaining)
    }
}

impl<S: Read> Read for DeadlineIo<S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.arm_timeout()?;
        self.socket.read(buffer)
    }
}

impl<S: Write> Write for DeadlineIo<S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.arm_timeout()?;
        self.socket.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.arm_timeout()?;
        self.socket.flush()
    }
}

fn protocol_request_before<S: Read + Write>(
    socket: S,
    request: &str,
    deadline: Instant,
    set_timeout: fn(&S, Duration) -> io::Result<()>,
) -> io::Result<String> {
    protocol_request(
        &mut DeadlineIo {
            socket,
            deadline,
            set_timeout,
        },
        request,
    )
}

fn protocol_request(socket: &mut (impl Read + Write), request: &str) -> io::Result<String> {
    if request.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ADB request exceeds limit",
        ));
    }
    write!(socket, "{:04x}{request}", request.len())?;
    let mut status = [0; 4];
    socket.read_exact(&mut status)?;
    if status != *b"OKAY" && status != *b"FAIL" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid host ADB status",
        ));
    }
    let mut length = [0; 4];
    socket.read_exact(&mut length)?;
    let length = std::str::from_utf8(&length)
        .ok()
        .and_then(|length| usize::from_str_radix(length, 16).ok())
        .filter(|length| *length as u64 <= MAX_OUTPUT)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid host ADB response length",
            )
        })?;
    let mut response = vec![0; length];
    socket.read_exact(&mut response)?;
    let response = String::from_utf8(response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if status == *b"FAIL" {
        return Err(io::Error::other(response));
    }
    Ok(response)
}

fn run_command(mut command: Command, timeout: Duration) -> io::Result<String> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(result.err().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::TimedOut, "host ADB command timed out")
                }));
            }
        }
    };
    if !status.success() {
        return Err(io::Error::other(format!(
            "host ADB command failed: {status}"
        )));
    }
    let mut output = String::new();
    child
        .stdout
        .take()
        .expect("piped ADB stdout")
        .take(MAX_OUTPUT + 1)
        .read_to_string(&mut output)?;
    if output.len() as u64 > MAX_OUTPUT {
        return Err(io::Error::other("host ADB output exceeds limit"));
    }
    Ok(output)
}

fn validate_public_key(key: &str) -> io::Result<()> {
    let lines: Vec<_> = key.lines().collect();
    let encoded = lines
        .first()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("");
    // Android's RSA-2048 public-key structure is 524 bytes, base64-encoded.
    if lines.len() != 1
        || encoded.len() != 700
        || !encoded.ends_with('=')
        || !encoded[..699]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
        || key.contains('\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ADB returned an invalid public key",
        ));
    }
    Ok(())
}

/// One optional transport registration, owned and stopped with the cell.
pub(crate) struct AdbConnection {
    stop: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl AdbConnection {
    /// Prepare the owner's key before Android init starts. Without the host's
    /// ADB package there is nothing to prepare; an unusable home directory
    /// disables only ADB, not Android boot.
    pub(crate) fn prepare(spec: &CellSpec) -> io::Result<Option<AdbClient>> {
        if !Path::new(ADB).is_file() {
            return Ok(None);
        }
        let client = AdbClient::for_uid(spec.host_uid)?;
        let key = client.public_key()?;
        let directory = spec.runtime_dir.join(DIRECTORY);
        fs::create_dir(&directory)?;
        fs::write(directory.join(KEYS_FILE), key)?;
        Ok(Some(client))
    }

    pub(crate) fn start(client: AdbClient) -> io::Result<Self> {
        Self::start_at(
            client,
            ENDPOINT.to_owned(),
            RETRY_INTERVAL,
            CONNECTED_INTERVAL,
        )
    }

    fn start_at(
        client: AdbClient,
        endpoint: String,
        interval: Duration,
        connected_interval: Duration,
    ) -> io::Result<Self> {
        Self::watch(endpoint.clone(), interval, connected_interval, move |operation| {
            if operation == "connect" {
                client.ensure_server()?;
            }
            client.transport_command(operation, &endpoint)
        })
    }

    /// Connect every `interval` until connected, then check every
    /// `connected_interval`.
    fn watch(
        endpoint: String,
        interval: Duration,
        connected_interval: Duration,
        mut transport_command: impl FnMut(&str) -> io::Result<String> + Send + 'static,
    ) -> io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("droidloom-adb".into())
            .spawn(move || {
                let mut connected = false;
                loop {
                    let online = transport_command("connect").is_ok_and(|output| {
                        output.starts_with("connected to ")
                            || output.starts_with("already connected to ")
                    });
                    if online && !connected {
                        eprintln!("droidloom-supervisor: ADB connected to {endpoint}");
                    }
                    connected = online;
                    let wait = if connected { connected_interval } else { interval };
                    if receiver.recv_timeout(wait) != Err(mpsc::RecvTimeoutError::Timeout) {
                        break;
                    }
                }
                // Disconnect only this cell. Other devices and the user's ADB
                // server outlive Droidloom and must keep running.
                if let Err(error) = transport_command("disconnect") {
                    eprintln!("droidloom-supervisor: ADB disconnect failed: {error}");
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for AdbConnection {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Force authentication even when the pinned development image disables it.
pub(crate) fn authenticated_properties(original: &str) -> String {
    let mut properties = String::new();
    for line in original.lines() {
        if line
            .split_once('=')
            .is_some_and(|(name, _)| matches!(name.trim(), "ro.adb.secure" | "ro.product.device"))
        {
            continue;
        }
        properties.push_str(line);
        properties.push('\n');
    }
    properties.push_str("ro.adb.secure=1\n");
    properties.push_str("ro.product.device=droidloom\n");
    properties
}

#[cfg(test)]
#[path = "adb_tests.rs"]
mod tests;

pub(crate) fn cleanup(runtime: &Path) -> io::Result<()> {
    let directory = runtime.join(DIRECTORY);
    for name in [KEYS_FILE, "init.rc", "build.prop"] {
        match fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    match fs::remove_dir(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
