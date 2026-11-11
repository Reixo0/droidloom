//! Android-side bounded audio control and C ABI for the upstream HAL adapter.
#![deny(unsafe_op_in_unsafe_fn)]

use droidloom_audio_protocol::*;
use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// One sample region retained until submit. Never retain its pointer afterward.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// Address in the dedicated pool.
    pub pointer: *mut u8,
    /// Valid capture frames or available playback capacity.
    pub frames: u32,
    slot: usize,
}

/// Audio progress reported by the host, using monotonic time.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Position {
    /// Playback frames presented, or captured frames returned to Android.
    pub frames: u64,
    /// Timestamp in CLOCK_MONOTONIC nanoseconds.
    pub monotonic_ns: u64,
    /// Estimated total latency in nanoseconds.
    pub latency_ns: u64,
    /// Discontinuity counter.
    pub discontinuities: u64,
}

/// A correlated control connection and its four immutable sample allocations.
pub struct Client {
    socket: SeqPacket,
    format: Format,
    buffers: Vec<SampleBuffer>,
    generation: u64,
    request: u32,
    region: Option<Region>,
    burst_position: bool,
    // Only snapshots received with a completed buffer operation are reusable.
    // Their original host timestamp is retained; control operations invalidate them.
    position_snapshot: Option<(Position, Instant)>,
}
// SAFETY: moving the entire client transfers its socket, mappings and any buffer
// credit together. It cannot be shared; dereferencing exported PCM pointers is unsafe.
unsafe impl Send for Client {}

impl Client {
    /// Stable backend node name for diagnostics and explicit lab graph links.
    pub fn node_name(&self) -> String {
        format!(
            "droidloom.{}.{:x}",
            if self.format.direction == Direction::Playback {
                "playback"
            } else {
                "capture"
            },
            self.generation
        )
    }
    /// Connect and verify server identity, stream format and descriptor access.
    pub fn connect(path: &Path, format: Format, role: Role) -> io::Result<Self> {
        // Older brokers reject OPEN features they don't know before creating a
        // stream: retry a fresh connection without the role, then without any.
        let tries = [BURST_POSITION | role.features(), BURST_POSITION, 0];
        let mut result = Err(io::Error::from_raw_os_error(libc::EPROTO));
        for features in tries {
            if role == Role::Media && features & STREAM_ROLE != 0 {
                continue;
            }
            result = Self::connect_with_features(path, format, features);
            if !matches!(&result, Err(error) if error.raw_os_error() == Some(libc::EPROTO)) {
                break;
            }
        }
        result
    }
    fn connect_with_features(path: &Path, format: Format, features: u64) -> io::Result<Self> {
        format.validate()?;
        let metadata = std::fs::symlink_metadata(path)?;
        let socket = SeqPacket::connect(path).map_err(ipc)?;
        if socket.peer_credentials().map_err(ipc)?.uid != metadata.uid() {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        socket_timeout(&socket, 3000)?;
        let open = Record {
            op: OPEN,
            request: 1,
            generation: 0,
            values: [
                format.direction as u64,
                u64::from(format.rate),
                u64::from(format.channels),
                u64::from(format.frames),
                features,
            ],
        };
        socket.send_record(&open.encode(), &[]).map_err(ipc)?;
        let mut bytes = [0; RECORD_BYTES];
        let received = socket.receive_record_into(&mut bytes).map_err(ipc)?;
        let record = Record::decode(received.bytes)?;
        check_error(record)?;
        if record.op != CONFIG
            || record.request != 1
            || record.generation == 0
            || record.values[..4]
                != [
                    u64::from(format.rate),
                    u64::from(format.channels),
                    u64::from(format.frames),
                    BUFFERS as u64,
                ]
            || record.values[4] & SHARED_POOL == 0
            || record.values[4] & !(SHARED_POOL | features) != 0
            || received.descriptors.len() != BUFFERS
        {
            return Err(invalid("invalid audio configuration or descriptors"));
        }
        let buffers = received
            .descriptors
            .into_iter()
            .map(|fd| {
                SampleBuffer::map(fd, format.bytes(), format.direction == Direction::Playback)
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            socket,
            format,
            buffers,
            generation: record.generation,
            request: 1,
            region: None,
            burst_position: record.values[4] & BURST_POSITION != 0,
            position_snapshot: None,
        })
    }
    fn exchange(&mut self, op: u32, values: [u64; 5]) -> io::Result<Record> {
        self.request = self
            .request
            .checked_add(1)
            .ok_or_else(|| invalid("audio request exhausted"))?;
        let record = Record {
            op,
            request: self.request,
            generation: self.generation,
            values,
        };
        self.socket
            .send_record(&record.encode(), &[])
            .map_err(ipc)?;
        let mut bytes = [0; RECORD_BYTES];
        let received = self.socket.receive_record_into(&mut bytes).map_err(ipc)?;
        let reply = Record::decode(received.bytes)?;
        if reply.request != self.request
            || reply.generation != self.generation
            || !received.descriptors.is_empty()
        {
            return Err(invalid(
                "uncorrelated audio reply or unexpected descriptors",
            ));
        }
        check_error(reply)?;
        Ok(reply)
    }
    /// Retain one pool slot. Access its pointer only until submit or client drop.
    pub fn acquire(&mut self, frames: u32) -> io::Result<Region> {
        if self.region.is_some() || frames == 0 {
            return Err(invalid("audio buffer already held or zero burst"));
        }
        self.position_snapshot = None;
        let op = if self.burst_position {
            ACQUIRE_POSITION
        } else {
            ACQUIRE
        };
        let reply = self.exchange(op, [u64::from(frames.min(self.format.frames)), 0, 0, 0, 0])?;
        if reply.op != op {
            return Err(invalid("invalid buffer response"));
        }
        let (slot, count, offset) = if self.burst_position {
            let (slot, count, offset) = unpack_credit(reply.values[0])?;
            (slot as usize, count, offset as usize)
        } else {
            if reply.values[3] != u64::from(self.format.frames) || reply.values[4] != 0 {
                return Err(invalid("invalid buffer response"));
            }
            (
                usize::try_from(reply.values[0]).map_err(|_| invalid("slot overflow"))?,
                u32::try_from(reply.values[1]).map_err(|_| invalid("frame overflow"))?,
                usize::try_from(reply.values[2]).map_err(|_| invalid("offset overflow"))?,
            )
        };
        if slot >= self.buffers.len()
            || count == 0
            || count > frames
            || count > self.format.frames
            || offset % self.format.frame_bytes() != 0
            || offset
                .checked_add(count as usize * self.format.frame_bytes())
                .is_none_or(|end| end > self.format.bytes())
        {
            return Err(invalid("audio region exceeds registered allocation"));
        }
        // SAFETY: the response bounds were validated against our immutable mapping.
        let pointer = unsafe { self.buffers[slot].pointer().add(offset) };
        let region = Region {
            pointer,
            frames: count,
            slot,
        };
        self.region = Some(region);
        if self.burst_position {
            self.position_snapshot = Some((
                Position {
                    frames: reply.values[1],
                    monotonic_ns: reply.values[2],
                    latency_ns: reply.values[3],
                    discontinuities: reply.values[4],
                },
                Instant::now(),
            ));
        }
        Ok(region)
    }
    /// Publish playback or return capture memory after all sample access finishes.
    pub fn submit(&mut self, frames: u32) -> io::Result<()> {
        let region = self
            .region
            .ok_or_else(|| invalid("no acquired audio buffer"))?;
        if frames > region.frames {
            return Err(invalid("submit exceeds buffer credit"));
        }
        self.position_snapshot = None;
        let op = if self.burst_position {
            SUBMIT_POSITION
        } else {
            SUBMIT
        };
        let reply = self.exchange(op, [region.slot as u64, u64::from(frames), 0, 0, 0])?;
        if self.burst_position {
            self.position_snapshot = Some((decode_position(reply)?, Instant::now()));
        } else if reply.op != ACK || reply.values != [0; 5] {
            return Err(invalid("invalid submit acknowledgement"));
        }
        self.region = None;
        Ok(())
    }
    /// Change stream lifecycle outside sample access.
    pub fn control(&mut self, command: u64) -> io::Result<()> {
        if self.region.is_some() {
            return Err(invalid("control while holding sample memory"));
        }
        self.position_snapshot = None;
        let reply = self.exchange(CONTROL, [command, 0, 0, 0, 0])?;
        if reply.op != ACK || reply.values != [0; 5] {
            return Err(invalid("invalid control acknowledgement"));
        }
        Ok(())
    }
    /// Query graph-derived progress and total buffering delay.
    pub fn position(&mut self) -> io::Result<Position> {
        // At most one 256-frame playback quantum of caching. Standalone
        // position polling still reaches the host once the snapshot expires.
        // Never extrapolate or manufacture a presentation timestamp here.
        let lifetime = Duration::from_nanos(
            u64::from(self.format.frames.min(256)) * 1_000_000_000 / u64::from(self.format.rate),
        );
        if let Some((position, received)) = self.position_snapshot {
            if received.elapsed() < lifetime {
                return Ok(position);
            }
        }
        self.position_snapshot = None;
        decode_position(self.exchange(STATUS, [0; 5])?)
    }
}

fn decode_position(reply: Record) -> io::Result<Position> {
    if reply.op != STATUS || reply.values[4] != 0 {
        return Err(invalid("invalid position response"));
    }
    Ok(Position {
        frames: reply.values[0],
        monotonic_ns: reply.values[1],
        latency_ns: reply.values[2],
        discontinuities: reply.values[3],
    })
}

fn ipc(error: IpcError) -> io::Error {
    match error {
        droidloom_audio_protocol::IpcError::Io(error) => error,
        error => io::Error::other(error.to_string()),
    }
}
fn check_error(record: Record) -> io::Result<()> {
    if record.op == ERROR {
        let code = i32::try_from(record.values[0])
            .ok()
            .filter(|code| (1..=4095).contains(code))
            .ok_or_else(|| invalid("invalid audio error code"))?;
        Err(io::Error::from_raw_os_error(code))
    } else {
        Ok(())
    }
}
fn status(result: io::Result<()>) -> i32 {
    result.map_or_else(
        |error| -error.raw_os_error().unwrap_or(libc::EPROTO),
        |()| 0,
    )
}

/// Open an opaque client for the C++ HAL. Output is null on any failure.
///
/// # Safety
/// path must be a live nul-terminated string; output must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_open(
    path: *const c_char,
    direction: u32,
    rate: u32,
    channels: u32,
    frames: u32,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: the same contract as dl_audio_open_role's.
    unsafe { dl_audio_open_role(path, direction, rate, channels, frames, 0, output) }
}

/// Like `dl_audio_open`, for a stream that carries one kind of sound: a
/// `Role` number, which the host gives a volume of its own.
///
/// # Safety
///
/// As for `dl_audio_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_open_role(
    path: *const c_char,
    direction: u32,
    rate: u32,
    channels: u32,
    frames: u32,
    role: u32,
    output: *mut *mut c_void,
) -> i32 {
    if path.is_null() || output.is_null() {
        return -libc::EINVAL;
    }
    // SAFETY: caller provides valid output storage and nul-terminated path.
    unsafe {
        *output = std::ptr::null_mut();
    }
    let result = (|| {
        // SAFETY: validity of path is part of the C ABI contract.
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|_| invalid("invalid audio path"))?;
        let role = Role::from_wire(u64::from(role)).ok_or_else(|| invalid("unknown audio role"))?;
        let client = Client::connect(
            Path::new(path),
            Format {
                direction: Direction::try_from(u64::from(direction))?,
                rate,
                channels,
                frames,
            },
            role,
        )?;
        // SAFETY: caller owns this box until dl_audio_close; output is writable.
        unsafe {
            *output = Box::into_raw(Box::new(client)).cast();
        }
        Ok(())
    })();
    status(result)
}

/// Release a client after every outstanding sample access has finished.
///
/// # Safety
/// handle must be null or an exclusively owned handle from dl_audio_open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_close(handle: *mut c_void) {
    if !handle.is_null() {
        // SAFETY: caller transfers the unique box ownership back to Rust.
        drop(unsafe { Box::from_raw(handle.cast::<Client>()) });
    }
}

/// Obtain a retained sample region without copying PCM.
///
/// # Safety
/// handle must be exclusively borrowed; pointer/count outputs must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_acquire(
    handle: *mut c_void,
    requested: u32,
    pointer: *mut *mut c_void,
    frames: *mut u32,
) -> i32 {
    if handle.is_null() || pointer.is_null() || frames.is_null() {
        return -libc::EINVAL;
    }
    status((|| {
        // SAFETY: pointers obey the C ABI ownership and output requirements.
        let region = unsafe { &mut *handle.cast::<Client>() }.acquire(requested)?;
        unsafe {
            *pointer = region.pointer.cast();
            *frames = region.frames;
        }
        Ok(())
    })())
}

/// End sample access and publish/return the retained region.
///
/// # Safety
/// handle must be a live, exclusively borrowed client handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_submit(handle: *mut c_void, frames: u32) -> i32 {
    if handle.is_null() {
        return -libc::EINVAL;
    }
    // SAFETY: exclusive handle ownership is part of the C ABI.
    status(unsafe { &mut *handle.cast::<Client>() }.submit(frames))
}

/// Apply a lifecycle operation.
///
/// # Safety
/// handle must be a live, exclusively borrowed client handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_control(handle: *mut c_void, command: u64) -> i32 {
    if handle.is_null() {
        return -libc::EINVAL;
    }
    // SAFETY: exclusive handle ownership is part of the C ABI.
    status(unsafe { &mut *handle.cast::<Client>() }.control(command))
}

/// Query timing without copying samples.
///
/// # Safety
/// handle must be exclusively borrowed and output must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_audio_position(handle: *mut c_void, output: *mut Position) -> i32 {
    if handle.is_null() || output.is_null() {
        return -libc::EINVAL;
    }
    status((|| {
        // SAFETY: the ABI provides a live handle and correctly sized output.
        let position = unsafe { &mut *handle.cast::<Client>() }.position()?;
        unsafe {
            *output = position;
        }
        Ok(())
    })())
}
