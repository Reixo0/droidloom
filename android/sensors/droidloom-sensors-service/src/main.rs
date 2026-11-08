//! Android sensors AIDL service for Droidloom: the host's sensors as
//! Android's.
//!
//! While Android has listeners, the service asks the host user's sensor
//! broker (`droidloom-sensors`, through the socket the supervisor projects at
//! `/dev/socket/droidloom/sensors`) for the host sensors they need and
//! reports each reading on Android's event queue. The host has an
//! accelerometer, a gyroscope, a light sensor, a proximity sensor and a
//! compass heading. Its raw magnetometer is of no use (the phone's own
//! magnets swamp it), so the magnetic field and the rotation vector Android
//! apps expect are made from the heading and gravity: the field is the
//! Earth's, as it would point given the heading.

use std::ffi::CStr;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use android_hardware_common_fmq::aidl::android::hardware::common::fmq::{
    MQDescriptor::MQDescriptor, SynchronizedReadWrite::SynchronizedReadWrite,
};
use android_hardware_sensors::aidl::android::hardware::sensors::{
    Event::{
        Event,
        EventPayload::{
            Data::Data,
            EventPayload,
            MetaData::{MetaData, MetaDataEventType::MetaDataEventType},
            Vec3::Vec3,
        },
    },
    ISensors::{
        self as sensors, BnSensors, ISensors, OperationMode::OperationMode, RateLevel::RateLevel,
        SharedMemInfo::SharedMemInfo,
    },
    ISensorsCallback::ISensorsCallback,
    SensorInfo::{self as info, SensorInfo},
    SensorStatus::SensorStatus,
    SensorType::SensorType,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong};
use droidloom_denial_ipc::SeqPacket;
use droidloom_sensors_protocol::{RECORD_BYTES, Record, Sample, Sensor as Host};
use fmq::MessageQueue;

const SERVICE_NAME: &str = "android.hardware.sensors.ISensors/default";
const BROKER: &str = "/dev/socket/droidloom/sensors";
/// The host publishes motion at most 30 times a second.
const MOTION_DELAY_US: i32 = 33_333;
/// What a far proximity reading says, in cm.
const PROXIMITY_FAR_CM: f32 = 5.0;
/// The Earth's field the magnetic field sensor reports, µT: its horizontal
/// part toward magnetic north, and its downward part.
const FIELD_NORTH_UT: f32 = 20.0;
const FIELD_DOWN_UT: f32 = 40.0;

/// Android's sensors, each with a handle and the host sensors it needs.
struct Kind {
    handle: i32,
    kind: SensorType,
    name: &'static str,
    needs: u32,
    max_range: f32,
    resolution: f32,
    min_delay_us: i32,
    flags: i32,
}

const ACCEL: i32 = 1;
const GYRO: i32 = 2;
const LIGHT: i32 = 3;
const PROXIMITY: i32 = 4;
const MAGNETIC: i32 = 5;
const ROTATION: i32 = 6;

const KINDS: [Kind; 6] = [
    Kind {
        handle: ACCEL,
        kind: SensorType::ACCELEROMETER,
        name: "Accelerometer",
        needs: Host::Accel.bit(),
        max_range: 78.4,
        resolution: 0.001,
        min_delay_us: MOTION_DELAY_US,
        flags: info::SENSOR_FLAG_BITS_CONTINUOUS_MODE,
    },
    Kind {
        handle: GYRO,
        kind: SensorType::GYROSCOPE,
        name: "Gyroscope",
        needs: Host::Gyro.bit(),
        max_range: 34.9,
        resolution: 0.001,
        min_delay_us: MOTION_DELAY_US,
        flags: info::SENSOR_FLAG_BITS_CONTINUOUS_MODE,
    },
    Kind {
        handle: LIGHT,
        kind: SensorType::LIGHT,
        name: "Ambient light",
        needs: Host::Light.bit(),
        max_range: 100_000.0,
        resolution: 1.0,
        min_delay_us: 0,
        flags: info::SENSOR_FLAG_BITS_ON_CHANGE_MODE,
    },
    // Android's default proximity sensor is a wake-up one.
    Kind {
        handle: PROXIMITY,
        kind: SensorType::PROXIMITY,
        name: "Proximity",
        needs: Host::Proximity.bit(),
        max_range: PROXIMITY_FAR_CM,
        resolution: PROXIMITY_FAR_CM,
        min_delay_us: 0,
        flags: info::SENSOR_FLAG_BITS_ON_CHANGE_MODE | info::SENSOR_FLAG_BITS_WAKE_UP,
    },
    Kind {
        handle: MAGNETIC,
        kind: SensorType::MAGNETIC_FIELD,
        name: "Magnetic field (from the compass heading)",
        needs: Host::Accel.bit() | Host::Heading.bit(),
        max_range: 2000.0,
        resolution: 0.01,
        min_delay_us: MOTION_DELAY_US,
        flags: info::SENSOR_FLAG_BITS_CONTINUOUS_MODE,
    },
    Kind {
        handle: ROTATION,
        kind: SensorType::ROTATION_VECTOR,
        name: "Rotation vector (from the compass heading)",
        needs: Host::Accel.bit() | Host::Heading.bit(),
        max_range: 1.0,
        resolution: 0.0001,
        min_delay_us: MOTION_DELAY_US,
        flags: info::SENSOR_FLAG_BITS_CONTINUOUS_MODE,
    },
];

fn kind(handle: i32) -> Option<&'static Kind> {
    KINDS.iter().find(|kind| kind.handle == handle)
}

fn handle_bit(handle: i32) -> u32 {
    1 << handle
}

/// Android's queues, from `initialize`.
struct Queues {
    events: MessageQueue<Event>,
    wake_lock: MessageQueue<i32>,
}

// SAFETY: the queues are shared memory mappings that only the service's
// lock ever reaches; nothing about them is tied to the creating thread.
unsafe impl Send for Queues {}

/// The latest host readings, for sensors made from several.
#[derive(Default)]
struct Latest {
    accel: Option<[f32; 3]>,
    heading: Option<(f32, u32)>,
    light: Option<f32>,
    near: Option<bool>,
}

#[derive(Default)]
struct State {
    queues: Option<Queues>,
    /// `handle_bit`s of the active sensors.
    active: u32,
    /// The broker connection while any sensor is active.
    broker: Option<Arc<SeqPacket>>,
    latest: Latest,
}

#[derive(Default)]
struct Sensors {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn bad_value() -> Status {
    Status::new_service_specific_error(sensors::ERROR_BAD_VALUE, None)
}

fn unsupported<T>() -> binder::Result<T> {
    Err(Status::new_exception(
        ExceptionCode::UNSUPPORTED_OPERATION,
        None,
    ))
}

/// Nanoseconds since boot, which Android's sensor events carry.
fn boottime_ns() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

fn event(handle: i32, payload: EventPayload) -> Event {
    let kind = kind(handle).map_or(SensorType::META_DATA, |kind| kind.kind);
    Event {
        timestamp: boottime_ns(),
        sensorHandle: handle,
        sensorType: kind,
        payload,
    }
}

fn vec3(handle: i32, [x, y, z]: [f32; 3], status: SensorStatus) -> Event {
    event(handle, EventPayload::Vec3(Vec3 { x, y, z, status }))
}

fn status(accuracy: u32) -> SensorStatus {
    match accuracy {
        0 => SensorStatus::UNRELIABLE,
        1 => SensorStatus::ACCURACY_LOW,
        2 => SensorStatus::ACCURACY_MEDIUM,
        _ => SensorStatus::ACCURACY_HIGH,
    }
}

type V = [f32; 3];

fn dot(a: V, b: V) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V, b: V) -> V {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn scale(a: V, s: f32) -> V {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn add(a: V, b: V) -> V {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn unit(a: V) -> Option<V> {
    let length = dot(a, a).sqrt();
    (length > 1e-3).then(|| scale(a, 1.0 / length))
}

/// East, north and up in the phone's axes, from gravity (the accelerometer
/// at rest points up) and the host's heading: degrees clockwise from north
/// that the top edge faces while the phone lies nearer flat, or the back
/// once it stands nearer upright.
fn frame(accel: V, heading_degrees: f32) -> Option<[V; 3]> {
    let up = unit(accel)?;
    let reference = if up[2].abs() >= std::f32::consts::FRAC_1_SQRT_2 {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, -1.0]
    };
    // The reference, laid flat, faces the heading; north is it turned back.
    let flat = unit(add(reference, scale(up, -dot(reference, up))))?;
    let (sin, cos) = heading_degrees.to_radians().sin_cos();
    let north = add(scale(flat, cos), scale(cross(up, flat), sin));
    let east = cross(north, up);
    Some([east, north, up])
}

/// Android's rotation vector for a frame: the unit quaternion turning the
/// phone's axes into east, north and up, x, y, z then w.
fn quaternion([east, north, up]: [V; 3]) -> [f32; 4] {
    // The rotation matrix has east, north and up as its rows.
    let m = [east, north, up];
    let trace = m[0][0] + m[1][1] + m[2][2];
    let (x, y, z, w) = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        (
            (m[2][1] - m[1][2]) / s,
            (m[0][2] - m[2][0]) / s,
            (m[1][0] - m[0][1]) / s,
            0.25 * s,
        )
    } else if m[0][0] > m[1][1] && m[0][0] > m[2][2] {
        let s = (1.0 + m[0][0] - m[1][1] - m[2][2]).sqrt() * 2.0;
        (
            0.25 * s,
            (m[0][1] + m[1][0]) / s,
            (m[0][2] + m[2][0]) / s,
            (m[2][1] - m[1][2]) / s,
        )
    } else if m[1][1] > m[2][2] {
        let s = (1.0 + m[1][1] - m[0][0] - m[2][2]).sqrt() * 2.0;
        (
            (m[0][1] + m[1][0]) / s,
            0.25 * s,
            (m[1][2] + m[2][1]) / s,
            (m[0][2] - m[2][0]) / s,
        )
    } else {
        let s = (1.0 + m[2][2] - m[0][0] - m[1][1]).sqrt() * 2.0;
        (
            (m[0][2] + m[2][0]) / s,
            (m[1][2] + m[2][1]) / s,
            0.25 * s,
            (m[1][0] - m[0][1]) / s,
        )
    };
    // Android keeps w positive.
    if w < 0.0 {
        [-x, -y, -z, -w]
    } else {
        [x, y, z, w]
    }
}

/// How far off the heading may be, in radians, for its accuracy.
fn heading_error(accuracy: u32) -> f32 {
    match accuracy {
        0 => std::f32::consts::PI,
        1 => 0.7,
        2 => 0.35,
        _ => 0.1,
    }
}

/// The events a host reading makes for the active sensors.
fn events(sample: Sample, active: u32, latest: &mut Latest) -> Vec<Event> {
    let on = |handle| active & handle_bit(handle) != 0;
    let values = [sample.x, sample.y, sample.z];
    let mut out = Vec::new();
    let mut derived = false;
    match sample.sensor {
        Host::Accel => {
            latest.accel = Some(values);
            if on(ACCEL) {
                out.push(vec3(ACCEL, values, SensorStatus::ACCURACY_HIGH));
            }
            derived = true;
        }
        Host::Gyro if on(GYRO) => out.push(vec3(GYRO, values, SensorStatus::ACCURACY_HIGH)),
        Host::Gyro => {}
        Host::Light => {
            latest.light = Some(sample.x);
            if on(LIGHT) {
                out.push(event(LIGHT, EventPayload::Scalar(sample.x)));
            }
        }
        Host::Proximity => {
            latest.near = Some(sample.x > 0.5);
            if on(PROXIMITY) {
                out.push(proximity(sample.x > 0.5));
            }
        }
        Host::Heading => {
            latest.heading = Some((sample.x, sample.accuracy));
            derived = true;
        }
    }
    if derived {
        out.extend(from_heading(active, latest));
    }
    out
}

fn proximity(near: bool) -> Event {
    let distance = if near { 0.0 } else { PROXIMITY_FAR_CM };
    event(PROXIMITY, EventPayload::Scalar(distance))
}

/// The magnetic field and rotation vector, when active and known.
fn from_heading(active: u32, latest: &Latest) -> Vec<Event> {
    let on = |handle| active & handle_bit(handle) != 0;
    let (Some(accel), Some((heading, accuracy))) = (latest.accel, latest.heading) else {
        return Vec::new();
    };
    let Some(frame) = frame(accel, heading) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if on(MAGNETIC) {
        let [_, north, up] = frame;
        let field = add(scale(north, FIELD_NORTH_UT), scale(up, -FIELD_DOWN_UT));
        out.push(vec3(MAGNETIC, field, status(accuracy)));
    }
    if on(ROTATION) {
        let [x, y, z, w] = quaternion(frame);
        let mut values = [0.0; 16];
        values[..5].copy_from_slice(&[x, y, z, w, heading_error(accuracy)]);
        out.push(event(ROTATION, EventPayload::Data(Data { values })));
    }
    out
}

/// What an on-change sensor reads now, for its first event.
fn current(handle: i32, latest: &Latest) -> Option<Event> {
    match handle {
        LIGHT => latest
            .light
            .map(|lux| event(LIGHT, EventPayload::Scalar(lux))),
        PROXIMITY => latest.near.map(proximity),
        _ => None,
    }
}

/// An event's bytes as Android's sensor service reads them: the C++
/// layout of a fixed-size AIDL parcelable, its union's tag sized and aligned
/// as the union. The generated Rust `WriteTo` of a fixed-size union cannot be
/// used: it places the variant by the address of a reference to it rather
/// than of the variant, writing it outside the event and leaving zeros.
fn raw_event(event: &Event) -> [u8; size_of::<Event>()] {
    let mut bytes = [0_u8; size_of::<Event>()];
    let mut put = |at: usize, value: &[u8]| bytes[at..at + value.len()].copy_from_slice(value);
    put(0, &event.timestamp.to_le_bytes());
    put(8, &event.sensorHandle.to_le_bytes());
    put(12, &event.sensorType.0.to_le_bytes());
    // The union's tag is its variant's index; the variant follows at the
    // union's alignment.
    let (tag, values): (u64, Vec<u8>) = match &event.payload {
        EventPayload::Vec3(vec) => {
            let mut values: Vec<u8> = [vec.x, vec.y, vec.z]
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect();
            values.push(vec.status.0 as u8);
            (0, values)
        }
        EventPayload::Meta(meta) => (3, meta.what.0.to_le_bytes().to_vec()),
        EventPayload::Scalar(value) => (4, value.to_le_bytes().to_vec()),
        EventPayload::Data(data) => (
            10,
            data.values.iter().flat_map(|value| value.to_le_bytes()).collect(),
        ),
        _ => (4, Vec::new()),
    };
    put(16, &tag.to_le_bytes());
    put(24, &values);
    bytes
}

// The payload starts after the 16-byte header and an 8-byte tag, and holds
// at least the 16 floats of `Data`.
const _: () = assert!(size_of::<Event>() >= 24 + 64 && align_of::<Event>() == 8);

/// Writes `events` to Android's queue and wakes its reader.
fn deliver(state: &mut State, events: Vec<Event>) {
    let Some(queues) = state.queues.as_mut() else {
        return;
    };
    // Android acknowledges wake-up events here; nothing holds a wake lock.
    while let Some(mut acknowledged) = queues.wake_lock.read() {
        let _ = acknowledged.read();
    }
    if events.is_empty() {
        return;
    }
    let Some(mut write) = queues.events.write_many(events.len()) else {
        eprintln!("Droidloom sensors: the event queue is full; dropped {}", events.len());
        return;
    };
    for (index, event) in events.iter().enumerate() {
        let target = write.ptr(index).cast::<u8>();
        for (offset, byte) in raw_event(event).into_iter().enumerate() {
            // SAFETY: `ptr` gives an aligned slot of `size_of::<Event>()`
            // bytes in the queue's shared memory, reserved by `write_many`.
            unsafe { target.add(offset).write_volatile(byte) };
        }
    }
    // SAFETY: every reserved slot was written above.
    unsafe { write.assume_written(events.len()) };
    drop(write);
    if let Some(word) = queues.events.event_flag_word() {
        wake(word, sensors::EVENT_QUEUE_FLAG_BITS_READ_AND_PROCESS as u32);
    }
}

/// Sets `bits` in an FMQ event flag and wakes its waiters, as libfmq's
/// `EventFlag::wake` does.
fn wake(word: &AtomicU32, bits: u32) {
    let old = word.fetch_or(bits, Ordering::SeqCst);
    if !old & bits != 0 {
        // SAFETY: the word lives in the queue's shared memory for as long as
        // the queue, which the caller's lock keeps alive.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE_BITSET,
                i32::MAX,
                std::ptr::null::<libc::timespec>(),
                std::ptr::null::<u32>(),
                bits,
            );
        }
    }
}

/// Reads the broker's samples and reports them until the connection ends.
fn follow(state: Arc<Mutex<State>>, broker: Arc<SeqPacket>) {
    let mut bytes = [0_u8; RECORD_BYTES];
    loop {
        let sample = match broker.receive_record_into(&mut bytes) {
            Ok(record) => match Record::decode(record.bytes) {
                Ok(Record::Sample(sample)) => sample,
                Ok(Record::Want(_)) | Err(_) => {
                    eprintln!("Droidloom sensors: unexpected record from the broker");
                    break;
                }
            },
            Err(error) => {
                eprintln!("Droidloom sensors: broker gone: {error}");
                break;
            }
        };
        let mut state = lock(&state);
        let active = state.active;
        let events = events(sample, active, &mut state.latest);
        deliver(&mut state, events);
    }
    let mut state = lock(&state);
    if state
        .broker
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &broker))
    {
        state.broker = None;
    }
}

impl Sensors {
    /// Tells the broker which host sensors the active ones need,
    /// connecting first.
    fn want(&self, state: &mut State) {
        let want = KINDS
            .iter()
            .filter(|kind| state.active & handle_bit(kind.handle) != 0)
            .fold(0, |want, kind| want | kind.needs);
        let broker = match &state.broker {
            Some(broker) => broker.clone(),
            None if want == 0 => return,
            None => match SeqPacket::connect(BROKER) {
                Ok(socket) => {
                    let socket = Arc::new(socket);
                    let reader = socket.clone();
                    let shared = self.state.clone();
                    let _ = std::thread::Builder::new()
                        .name("sensors-broker".to_owned())
                        .spawn(move || follow(shared, reader));
                    state.broker = Some(socket.clone());
                    socket
                }
                Err(error) => {
                    eprintln!("Droidloom sensors: no sensor broker: {error}");
                    return;
                }
            },
        };
        let _ = broker.send_record(&Record::Want(want).encode(), &[]);
    }
}

impl Interface for Sensors {
    fn dump(&self, writer: &mut dyn Write, _args: &[&CStr]) -> Result<(), binder::StatusCode> {
        let state = lock(&self.state);
        writeln!(
            writer,
            "active {:#x} broker {} queues {}",
            state.active,
            state.broker.is_some(),
            state.queues.is_some()
        )
        .map_err(|_| binder::StatusCode::UNKNOWN_ERROR)
    }
}

#[allow(non_snake_case, reason = "AIDL method names")]
impl ISensors for Sensors {
    fn activate(&self, handle: i32, enabled: bool) -> binder::Result<()> {
        kind(handle).ok_or_else(bad_value)?;
        let mut state = lock(&self.state);
        if enabled {
            state.active |= handle_bit(handle);
        } else {
            state.active &= !handle_bit(handle);
        }
        self.want(&mut state);
        if enabled {
            if let Some(first) = current(handle, &state.latest) {
                deliver(&mut state, vec![first]);
            }
        }
        Ok(())
    }

    fn batch(&self, handle: i32, _sampling_ns: i64, _latency_ns: i64) -> binder::Result<()> {
        // The host sets its own rates; readings come as they change.
        kind(handle).map(|_| ()).ok_or_else(bad_value)
    }

    fn configDirectReport(&self, _handle: i32, _channel: i32, _rate: RateLevel) -> binder::Result<i32> {
        unsupported()
    }

    fn flush(&self, handle: i32) -> binder::Result<()> {
        kind(handle).ok_or_else(bad_value)?;
        let complete = Event {
            timestamp: 0,
            sensorHandle: handle,
            sensorType: SensorType::META_DATA,
            payload: EventPayload::Meta(MetaData {
                what: MetaDataEventType::META_DATA_FLUSH_COMPLETE,
            }),
        };
        deliver(&mut lock(&self.state), vec![complete]);
        Ok(())
    }

    fn getSensorsList(&self) -> binder::Result<Vec<SensorInfo>> {
        Ok(KINDS
            .iter()
            .map(|kind| SensorInfo {
                sensorHandle: kind.handle,
                name: kind.name.to_owned(),
                vendor: "Droidloom (the host's sensors)".to_owned(),
                version: 1,
                r#type: kind.kind,
                typeAsString: String::new(),
                maxRange: kind.max_range,
                resolution: kind.resolution,
                power: 0.1,
                minDelayUs: kind.min_delay_us,
                fifoReservedEventCount: 0,
                fifoMaxEventCount: 0,
                requiredPermission: String::new(),
                maxDelayUs: if kind.min_delay_us > 0 { 1_000_000 } else { 0 },
                flags: kind.flags,
            })
            .collect())
    }

    fn initialize(
        &self,
        events: &MQDescriptor<Event, SynchronizedReadWrite>,
        wake_lock: &MQDescriptor<i32, SynchronizedReadWrite>,
        _callback: &Strong<dyn ISensorsCallback>,
    ) -> binder::Result<()> {
        let mut state = lock(&self.state);
        // A new sensor service starts with every sensor off.
        state.active = 0;
        self.want(&mut state);
        state.queues = Some(Queues {
            events: MessageQueue::from_desc(events, true),
            wake_lock: MessageQueue::from_desc(wake_lock, true),
        });
        Ok(())
    }

    fn injectSensorData(&self, _event: &Event) -> binder::Result<()> {
        unsupported()
    }

    fn registerDirectChannel(&self, _memory: &SharedMemInfo) -> binder::Result<i32> {
        unsupported()
    }

    fn setOperationMode(&self, mode: OperationMode) -> binder::Result<()> {
        if mode == OperationMode::NORMAL {
            Ok(())
        } else {
            unsupported()
        }
    }

    fn unregisterDirectChannel(&self, _channel: i32) -> binder::Result<()> {
        unsupported()
    }
}

fn main() {
    ProcessState::set_thread_pool_max_thread_count(2);
    ProcessState::start_thread_pool();
    let service = BnSensors::new_binder(Sensors::default(), BinderFeatures::default());
    if let Err(error) = binder::add_service(SERVICE_NAME, service.as_binder()) {
        eprintln!("Droidloom sensors fatal: register {SERVICE_NAME}: {error:?}");
        std::process::exit(1);
    }
    ProcessState::join_thread_pool();
}

#[cfg(test)]
mod tests {
    use super::{dot, frame, quaternion};

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < 1e-4)
    }

    #[test]
    fn a_flat_phone_facing_north_is_the_world() {
        let [east, north, up] = frame([0.0, 0.0, 9.8], 0.0).unwrap();
        assert!(close(east, [1.0, 0.0, 0.0]));
        assert!(close(north, [0.0, 1.0, 0.0]));
        assert!(close(up, [0.0, 0.0, 1.0]));
        let q = quaternion([east, north, up]);
        assert!((q[3] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn a_flat_phone_facing_east_has_north_on_its_left() {
        let [east, north, _] = frame([0.0, 0.0, 9.8], 90.0).unwrap();
        assert!(close(north, [-1.0, 0.0, 0.0]));
        assert!(close(east, [0.0, 1.0, 0.0]));
    }

    #[test]
    fn an_upright_phone_uses_its_back() {
        // Standing in portrait, the back facing north.
        let [east, north, up] = frame([0.0, 9.8, 0.0], 0.0).unwrap();
        assert!(close(up, [0.0, 1.0, 0.0]));
        assert!(close(north, [0.0, 0.0, -1.0]));
        assert!(close(east, [1.0, 0.0, 0.0]));
        assert!(dot(east, north).abs() < 1e-4);
    }
}
