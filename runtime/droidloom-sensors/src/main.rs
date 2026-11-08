//! Per-user sensor broker for Android's sensors HAL.
//!
//! The HAL connects to the socket the supervisor projects at
//! `/dev/socket/droidloom/sensors` and says which host sensors its listeners
//! need. The broker claims them from steinerd, the host's sensor service,
//! for exactly as long, and forwards each new reading. Every HAL connection
//! has its own bus connection, so steinerd ends the claims when it goes.

use std::collections::HashMap;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use clap::Parser;
use droidloom_denial_ipc::{SeqPacket, SeqPacketListener};
use droidloom_sensors_protocol::{RECORD_BYTES, Record, Sample, Sensor};
use zbus::blocking::{Connection, Proxy, fdo::PropertiesProxy};
use zbus::zvariant::OwnedValue;

const STEINERD: &str = "org.steinerd";
const STEINERD_PATH: &str = "/org/steinerd";
const SENSORS: &str = "org.steinerd.Sensors1";

#[derive(Parser)]
#[command(about = "Per-user steinerd bridge for Android's sensors")]
struct Arguments {
    /// Delegated socket; parent must be owned by this user and mode 0700.
    #[arg(long)]
    socket: PathBuf,
    /// Host-mapped UID of Android's sensors HAL (the system user, 1000).
    #[arg(long, default_value_t = 1000)]
    peer_uid: u32,
}

fn main() {
    if let Err(error) = run(Arguments::parse()) {
        eprintln!("droidloom-sensors: {error}");
        std::process::exit(1);
    }
}

fn run(arguments: Arguments) -> io::Result<()> {
    // SAFETY: credential query has no side effects.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        return Err(io::Error::other("sensors must run as the host user"));
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
            "sensors socket parent must be user-owned and mode 0700",
        ));
    }
    let activated = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        == Some(std::process::id());
    let listener = if activated {
        if std::env::var("LISTEN_FDS").as_deref() != Ok("1") {
            return Err(io::Error::other(
                "sensors needs exactly one activation socket",
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
                eprintln!("droidloom-sensors: accept: {error}");
                continue;
            }
        };
        let peer_uid = arguments.peer_uid;
        std::thread::spawn(move || {
            if let Err(error) = serve(socket, peer_uid) {
                eprintln!("droidloom-sensors: {error}");
            }
        });
    }
}

/// steinerd's name for a host sensor.
fn claim_name(sensor: Sensor) -> &'static str {
    match sensor {
        Sensor::Accel => "accel",
        Sensor::Gyro => "gyro",
        Sensor::Light => "light",
        Sensor::Proximity => "proximity",
        Sensor::Heading => "compass",
    }
}

/// One HAL connection: claims follow what it wants.
fn serve(socket: SeqPacket, peer_uid: u32) -> io::Result<()> {
    let peer = socket.peer_credentials().map_err(io::Error::other)?;
    if peer.uid != peer_uid {
        return Err(io::Error::other(format!(
            "refused peer uid {} (expected {peer_uid})",
            peer.uid
        )));
    }
    let socket = Arc::new(socket);
    let bus = Connection::system().map_err(io::Error::other)?;
    let steinerd = Proxy::new(&bus, STEINERD, STEINERD_PATH, SENSORS).map_err(io::Error::other)?;
    let wanted = Arc::new(AtomicU32::new(0));
    let readings = Arc::new(Mutex::new(Readings::default()));
    {
        let (bus, socket, wanted, readings) =
            (bus.clone(), socket.clone(), wanted.clone(), readings.clone());
        std::thread::spawn(move || {
            if let Err(error) = forward(&bus, &socket, &wanted, &readings) {
                eprintln!("droidloom-sensors: steinerd: {error}");
            }
        });
    }

    let mut claimed = 0_u32;
    let mut bytes = [0_u8; RECORD_BYTES];
    let result = loop {
        let record = match socket.receive_record_into(&mut bytes) {
            Ok(record) => Record::decode(record.bytes),
            // The HAL went away.
            Err(_) => break Ok(()),
        };
        let want = match record {
            Ok(Record::Want(want)) => want,
            Ok(Record::Sample(_)) => break Err(io::Error::other("the HAL sent a host record")),
            Err(error) => break Err(error),
        };
        wanted.store(want, Ordering::SeqCst);
        for sensor in Sensor::ALL {
            let bit = sensor.bit();
            if want & bit != 0 && claimed & bit == 0 {
                match steinerd.call_method("Claim", &(claim_name(sensor),)) {
                    Ok(_) => {
                        claimed |= bit;
                        // On-change sensors start with what they read now.
                        if matches!(sensor, Sensor::Light | Sensor::Proximity | Sensor::Heading) {
                            let current = current(&steinerd, sensor, &readings);
                            if let Some(sample) = current {
                                let _ = socket.send_record(&Record::Sample(sample).encode(), &[]);
                            }
                        }
                    }
                    Err(error) => eprintln!(
                        "droidloom-sensors: claim {}: {error}",
                        claim_name(sensor)
                    ),
                }
            } else if want & bit == 0 && claimed & bit != 0 {
                let _ = steinerd.call_method("Release", &(claim_name(sensor),));
                claimed &= !bit;
            }
        }
    };
    for sensor in Sensor::ALL {
        if claimed & sensor.bit() != 0 {
            let _ = steinerd.call_method("Release", &(claim_name(sensor),));
        }
    }
    // Ends the forwarding thread's signal stream and the claims with it.
    let _ = bus.close();
    result
}

/// What the HAL needs besides each property's own value.
#[derive(Default)]
struct Readings {
    heading: Option<f64>,
    heading_accuracy: u8,
}

impl Readings {
    fn heading_sample(&self) -> Option<Sample> {
        Some(Sample {
            sensor: Sensor::Heading,
            x: self.heading? as f32,
            y: 0.0,
            z: 0.0,
            accuracy: u32::from(self.heading_accuracy),
        })
    }
}

/// A sensor's current reading from steinerd's properties.
fn current(steinerd: &Proxy<'_>, sensor: Sensor, readings: &Mutex<Readings>) -> Option<Sample> {
    let property = |name: &str| steinerd.get_property::<OwnedValue>(name).ok();
    let mut changed = HashMap::new();
    match sensor {
        Sensor::Light => {
            changed.insert("Light".to_owned(), property("Light")?);
        }
        Sensor::Proximity => {
            changed.insert("Near".to_owned(), property("Near")?);
        }
        Sensor::Heading => {
            changed.insert("Heading".to_owned(), property("Heading")?);
            changed.insert("HeadingAccuracy".to_owned(), property("HeadingAccuracy")?);
        }
        Sensor::Accel | Sensor::Gyro => return None,
    }
    let mut readings = readings.lock().unwrap_or_else(PoisonError::into_inner);
    samples(&changed, &mut readings)
        .into_iter()
        .find(|sample| sample.sensor == sensor)
}

/// The samples a change of steinerd's properties makes.
fn samples(changed: &HashMap<String, OwnedValue>, readings: &mut Readings) -> Vec<Sample> {
    let triple = |name: &str| {
        changed
            .get(name)
            .and_then(|value| <(f64, f64, f64)>::try_from(value.try_clone().ok()?).ok())
    };
    let sample = |sensor, (x, y, z): (f64, f64, f64)| Sample {
        sensor,
        x: x as f32,
        y: y as f32,
        z: z as f32,
        accuracy: 3,
    };
    let mut out = Vec::new();
    if let Some(values) = triple("Acceleration") {
        out.push(sample(Sensor::Accel, values));
    }
    if let Some(values) = triple("RotationRate") {
        out.push(sample(Sensor::Gyro, values));
    }
    if let Some(lux) = changed.get("Light").and_then(|value| f64::try_from(value).ok()) {
        out.push(sample(Sensor::Light, (lux, 0.0, 0.0)));
    }
    if let Some(near) = changed.get("Near").and_then(|value| bool::try_from(value).ok()) {
        out.push(sample(Sensor::Proximity, (f64::from(u8::from(near)), 0.0, 0.0)));
    }
    let heading = changed.get("Heading").and_then(|value| f64::try_from(value).ok());
    let accuracy = changed
        .get("HeadingAccuracy")
        .and_then(|value| u8::try_from(value).ok());
    if let Some(accuracy) = accuracy {
        readings.heading_accuracy = accuracy.min(3);
    }
    if let Some(heading) = heading {
        // steinerd says -1 before its first reading.
        readings.heading = (heading >= 0.0).then_some(heading);
    }
    if heading.is_some() || accuracy.is_some() {
        out.extend(readings.heading_sample());
    }
    out
}

/// Forwards steinerd's changes of wanted sensors until the bus or the HAL
/// goes.
fn forward(
    bus: &Connection,
    socket: &SeqPacket,
    wanted: &AtomicU32,
    readings: &Mutex<Readings>,
) -> zbus::Result<()> {
    let properties = PropertiesProxy::builder(bus)
        .destination(STEINERD)?
        .path(STEINERD_PATH)?
        .build()?;
    for signal in properties.receive_properties_changed()? {
        let arguments = signal.args()?;
        if arguments.interface_name.as_str() != SENSORS {
            continue;
        }
        let changed: HashMap<String, OwnedValue> = arguments
            .changed_properties
            .iter()
            .filter_map(|(name, value)| Some(((*name).to_owned(), value.try_to_owned().ok()?)))
            .collect();
        let samples = {
            let mut readings = readings.lock().unwrap_or_else(PoisonError::into_inner);
            samples(&changed, &mut readings)
        };
        let want = wanted.load(Ordering::SeqCst);
        for sample in samples {
            if want & sample.sensor.bit() != 0
                && socket
                    .send_record(&Record::Sample(sample).encode(), &[])
                    .is_err()
            {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Readings, samples};
    use droidloom_sensors_protocol::Sensor;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    fn value(value: Value<'_>) -> OwnedValue {
        value.try_to_owned().unwrap()
    }

    #[test]
    fn changes_become_samples() {
        let changed = HashMap::from([
            (
                "Acceleration".to_owned(),
                value(Value::from((0.5_f64, 9.75_f64, 0.25_f64))),
            ),
            ("Near".to_owned(), value(Value::Bool(true))),
            ("Orientation".to_owned(), value(Value::from("normal"))),
        ]);
        let out = samples(&changed, &mut Readings::default());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sensor, Sensor::Accel);
        assert_eq!((out[0].y, out[1].sensor, out[1].x), (9.75, Sensor::Proximity, 1.0));
    }

    #[test]
    fn heading_waits_for_a_reading_and_keeps_its_accuracy() {
        let mut readings = Readings::default();
        let before = HashMap::from([("Heading".to_owned(), value(Value::F64(-1.0)))]);
        assert!(samples(&before, &mut readings).is_empty());
        let accuracy = HashMap::from([("HeadingAccuracy".to_owned(), value(Value::U8(2)))]);
        assert!(samples(&accuracy, &mut readings).is_empty());
        let heading = HashMap::from([("Heading".to_owned(), value(Value::F64(90.0)))]);
        let out = samples(&heading, &mut readings);
        assert_eq!((out[0].x, out[0].accuracy), (90.0, 2));
    }
}
