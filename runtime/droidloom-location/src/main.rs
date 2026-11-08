//! Per-user location broker for Android's GNSS HAL.
//!
//! The HAL connects to the socket the supervisor projects at
//! `/dev/socket/droidloom/location` and sends `START` while an Android app
//! wants location. The broker then asks geoclue, as the desktop app
//! `droidloom`, and forwards each new fix until `STOP` or the socket closes.
//! geoclue applies the user's location switch and rules, and the desktop's
//! indicator shows Droidloom using location; when it refuses, the HAL hears
//! `DENIED`.
//!
//! Fixes are read from the geoclue client's `Location` property once per
//! interval rather than from its signal, so a session is a plain thread that
//! stops at its next tick.

use std::collections::HashMap;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use clap::Parser;
use droidloom_denial_ipc::{SeqPacket, SeqPacketListener};
use droidloom_location_protocol::{
    Fix, HAS_ALTITUDE, HAS_BEARING, HAS_SPEED, RECORD_BYTES, Record,
};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const GEOCLUE: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER: &str = "org.freedesktop.GeoClue2.Manager";
const CLIENT: &str = "org.freedesktop.GeoClue2.Client";
const LOCATION: &str = "org.freedesktop.GeoClue2.Location";
/// How Android shows up in the desktop's privacy lists.
const DESKTOP_ID: &str = "droidloom";
/// `GClueAccuracyLevel` exact.
const ACCURACY_EXACT: u32 = 8;
/// Bounds on how often a session looks for a new fix.
const MIN_INTERVAL_MS: u32 = 1000;
const MAX_INTERVAL_MS: u32 = 60_000;

#[derive(Parser)]
#[command(about = "Per-user geoclue bridge for Android's location")]
struct Arguments {
    /// Delegated socket; parent must be owned by this user and mode 0700.
    #[arg(long)]
    socket: PathBuf,
    /// Host-mapped UID of Android's GNSS HAL (the gps user, 1021).
    #[arg(long, default_value_t = 1021)]
    peer_uid: u32,
}

fn main() {
    if let Err(error) = run(Arguments::parse()) {
        eprintln!("droidloom-location: {error}");
        std::process::exit(1);
    }
}

fn run(arguments: Arguments) -> io::Result<()> {
    // SAFETY: credential query has no side effects.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        return Err(io::Error::other("location must run as the host user"));
    }
    let parent = arguments
        .socket
        .parent()
        .ok_or_else(|| io::Error::other("socket needs a parent"))?;
    if !parent.exists() {
        std::fs::create_dir(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::other(
            "location socket parent must be user-owned and mode 0700",
        ));
    }
    let activated = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        == Some(std::process::id());
    let listener = if activated {
        if std::env::var("LISTEN_FDS").as_deref() != Ok("1") {
            return Err(io::Error::other(
                "location needs exactly one activation socket",
            ));
        }
        // SAFETY: mark the inherited listener close-on-exec before owning it.
        if unsafe { libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: systemd's matching PID and single descriptor transfer FD 3
        // to this process; the adapter verifies it is a listening seqpacket.
        let fd = unsafe { OwnedFd::from_raw_fd(3) };
        SeqPacketListener::from_owned_fd(fd).map_err(io::Error::other)?
    } else {
        let listener = SeqPacketListener::bind(&arguments.socket).map_err(io::Error::other)?;
        // The supervisor projects only this inode into Android, where the
        // HAL's UID needs access; the parent protects the host path.
        std::fs::set_permissions(&arguments.socket, std::fs::Permissions::from_mode(0o666))?;
        listener
    };
    loop {
        let socket = match listener.accept() {
            Ok(socket) => socket,
            Err(error) => {
                eprintln!("droidloom-location: accept: {error}");
                continue;
            }
        };
        let peer_uid = arguments.peer_uid;
        std::thread::spawn(move || {
            if let Err(error) = serve(socket, peer_uid) {
                eprintln!("droidloom-location: {error}");
            }
        });
    }
}

/// One HAL connection: sessions start and stop as it asks.
fn serve(socket: SeqPacket, peer_uid: u32) -> io::Result<()> {
    let peer = socket.peer_credentials().map_err(io::Error::other)?;
    if peer.uid != peer_uid {
        return Err(io::Error::other(format!(
            "refused peer uid {} (expected {peer_uid})",
            peer.uid
        )));
    }
    let socket = Arc::new(socket);
    let mut session: Option<Session> = None;
    let mut bytes = [0_u8; RECORD_BYTES];
    loop {
        let record = match socket.receive_record_into(&mut bytes) {
            Ok(record) => Record::decode(record.bytes)?,
            // The HAL went away: stop whatever runs for it.
            Err(_) => break,
        };
        match record {
            Record::Start(interval_ms) => {
                drop(session.take());
                session = Some(Session::start(socket.clone(), interval_ms));
            }
            Record::Stop => drop(session.take()),
            Record::Fix(_) | Record::Denied => {
                return Err(io::Error::other("the HAL sent a host record"));
            }
        }
    }
    Ok(())
}

/// A geoclue client running for Android, until dropped.
struct Session {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    fn start(socket: Arc<SeqPacket>, interval_ms: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let interval = Duration::from_millis(u64::from(
            interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS),
        ));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            if let Err(error) = follow(&socket, interval, &stopped) {
                eprintln!("droidloom-location: geoclue: {error}");
                let _ = socket.send_record(&Record::Denied.encode(), &[]);
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Runs a geoclue client and sends each new fix until `stop`.
fn follow(socket: &SeqPacket, interval: Duration, stop: &AtomicBool) -> zbus::Result<()> {
    let bus = Connection::system()?;
    let manager = Proxy::new(&bus, GEOCLUE, MANAGER_PATH, MANAGER)?;
    let path: OwnedObjectPath = manager.call("CreateClient", &())?;
    let client = Proxy::new(&bus, GEOCLUE, path.clone(), CLIENT)?;
    client.set_property("DesktopId", DESKTOP_ID)?;
    client.set_property("RequestedAccuracyLevel", ACCURACY_EXACT)?;
    client.set_property("DistanceThreshold", 0_u32)?;
    let seconds = u32::try_from(interval.as_secs()).unwrap_or(u32::MAX);
    client.set_property("TimeThreshold", seconds.saturating_sub(1))?;
    client.call_method("Start", &())?;

    let mut last = OwnedObjectPath::default();
    while !stop.load(Ordering::SeqCst) {
        let location: OwnedObjectPath = client.get_property("Location")?;
        if location.as_str() != "/" && location != last {
            last = location.clone();
            let properties = Proxy::new(
                &bus,
                GEOCLUE,
                location,
                "org.freedesktop.DBus.Properties",
            )?;
            let all: HashMap<String, OwnedValue> = properties.call("GetAll", &(LOCATION,))?;
            if let Some(fix) = fix_from(&all) {
                let sent = socket.send_record(&Record::Fix(fix).encode(), &[]);
                if sent.is_err() {
                    break;
                }
            }
        }
        // Short naps so a stop is quick.
        let mut slept = Duration::ZERO;
        while slept < interval && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
            slept += Duration::from_millis(100);
        }
    }

    let _ = client.call_method("Stop", &());
    let _ = manager.call_method("DeleteClient", &(path,));
    Ok(())
}

fn number(all: &HashMap<String, OwnedValue>, name: &str) -> Option<f64> {
    all.get(name).and_then(|value| f64::try_from(value).ok())
}

/// A fix from a geoclue `Location`'s properties.
#[allow(clippy::cast_possible_truncation, reason = "metres, speeds and degrees fit in f32")]
fn fix_from(all: &HashMap<String, OwnedValue>) -> Option<Fix> {
    let mut fix = Fix {
        latitude: number(all, "Latitude")?,
        longitude: number(all, "Longitude")?,
        accuracy: number(all, "Accuracy")? as f32,
        ..Fix::default()
    };
    // geoclue says "unknown" with the lowest double for altitude and with a
    // negative speed or heading.
    if let Some(altitude) = number(all, "Altitude").filter(|a| *a > -1.0e300) {
        fix.altitude = altitude;
        fix.flags |= HAS_ALTITUDE;
    }
    if let Some(speed) = number(all, "Speed").filter(|s| *s >= 0.0) {
        fix.speed = speed as f32;
        fix.flags |= HAS_SPEED;
    }
    if let Some(bearing) = number(all, "Heading").filter(|h| *h >= 0.0) {
        fix.bearing = bearing as f32;
        fix.flags |= HAS_BEARING;
    }
    let (seconds, micros) = all
        .get("Timestamp")
        .and_then(|value| <(u64, u64)>::try_from(value.clone()).ok())
        .unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            (now.as_secs(), u64::from(now.subsec_micros()))
        });
    fix.time_ms = i64::try_from(seconds * 1000 + micros / 1000).ok()?;
    Some(fix)
}

#[cfg(test)]
mod tests {
    use super::{HAS_ALTITUDE, HAS_SPEED, fix_from};
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    fn value(value: Value<'_>) -> OwnedValue {
        value.try_to_owned().unwrap()
    }

    #[test]
    fn unknowns_stay_unknown() {
        let all = HashMap::from([
            ("Latitude".to_owned(), value(Value::F64(40.4))),
            ("Longitude".to_owned(), value(Value::F64(-3.7))),
            ("Accuracy".to_owned(), value(Value::F64(5.0))),
            ("Altitude".to_owned(), value(Value::F64(-f64::MAX))),
            ("Speed".to_owned(), value(Value::F64(2.0))),
            ("Heading".to_owned(), value(Value::F64(-1.0))),
            (
                "Timestamp".to_owned(),
                value(Value::from((1_794_000_000_u64, 250_000_u64))),
            ),
        ]);
        let fix = fix_from(&all).unwrap();
        assert_eq!(fix.flags, HAS_SPEED);
        assert_eq!(fix.flags & HAS_ALTITUDE, 0);
        assert_eq!(fix.time_ms, 1_794_000_000_250);
    }

    #[test]
    fn a_fix_needs_a_position() {
        assert!(fix_from(&HashMap::new()).is_none());
    }
}
