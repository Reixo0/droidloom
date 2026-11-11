//! Private PCM16 stream control. Samples travel only in dedicated sealed-size memfds.
#![deny(unsafe_op_in_unsafe_fn)]

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

pub use droidloom_denial_ipc::{IpcError, PeerCredentials, SeqPacket, SeqPacketListener};

/// Fixed record size; neither control packets nor ancillary metadata contain PCM.
pub const RECORD_BYTES: usize = 64;
/// Dedicated buffers per stream, fitting the audited transport's descriptor limit.
pub const BUFFERS: usize = 4;
/// Maximum frames in one allocation.
pub const MAX_FRAMES: u32 = 8192;
/// Supported PCM sample rate.
pub const RATE: u32 = 48_000;
/// Open a stream and register its memory.
pub const OPEN: u32 = 1;
/// Stream configuration and four sample descriptors.
pub const CONFIG: u32 = 2;
/// Acquire one buffer with exclusive access until SUBMIT.
pub const ACQUIRE: u32 = 3;
/// Submit playback or return consumed capture memory.
pub const SUBMIT: u32 = 4;
/// Apply one stream operation.
pub const CONTROL: u32 = 5;
/// Query position, latency and discontinuity count.
pub const STATUS: u32 = 6;
/// Negative errno response.
pub const ERROR: u32 = 7;
/// Successful operation response.
pub const ACK: u32 = 8;
/// Acquire a buffer and a coherent host position snapshot in one exchange.
pub const ACQUIRE_POSITION: u32 = 9;
/// Submit a buffer and return its host position snapshot instead of an empty ACK.
pub const SUBMIT_POSITION: u32 = 10;
/// Enable graph processing.
pub const START: u64 = 1;
/// Pause graph processing without discarding queued data.
pub const PAUSE: u64 = 2;
/// Discard queued data.
pub const FLUSH: u64 = 3;
/// Wait for playback to drain.
pub const DRAIN: u64 = 4;
/// Stop graph processing and discard queued data.
pub const STANDBY: u64 = 5;
/// Shared pool accepted directly by PipeWire; no sample copy in the broker.
pub const SHARED_POOL: u64 = 1;
/// Optional OPEN/CONFIG feature: timing snapshots on buffer exchanges.
pub const BURST_POSITION: u64 = 1 << 1;
/// OPEN feature: the stream names the kind of sound it carries, in
/// `ROLE_MASK`. Without it a stream is media.
pub const STREAM_ROLE: u64 = 1 << 2;
const ROLE_SHIFT: u32 = 8;
/// OPEN feature field: the stream's `Role`.
pub const ROLE_MASK: u64 = 0xf << ROLE_SHIFT;

/// The kind of sound a playback stream carries, which the host gives a
/// volume of its own: Android's audio policy sends each kind to its own
/// output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Role {
    /// Music, video, games and anything else.
    Media = 0,
    /// Notifications and other short event sounds.
    Notification = 1,
    /// An incoming call ringing.
    Ringtone = 2,
    /// Alarms.
    Alarm = 3,
    /// Voice and video calls.
    Call = 4,
}

impl Role {
    const ALL: [Self; 5] = [
        Self::Media,
        Self::Notification,
        Self::Ringtone,
        Self::Alarm,
        Self::Call,
    ];

    /// A role from its number, as the HAL passes it.
    pub fn from_wire(value: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|role| *role as u64 == value)
    }

    /// OPEN's feature bits for this role.
    pub fn features(self) -> u64 {
        STREAM_ROLE | ((self as u64) << ROLE_SHIFT)
    }

    /// PipeWire's media.role, which the host's session manager routes by.
    pub fn media_role(self) -> &'static str {
        match self {
            Self::Media => "Music",
            Self::Notification => "Notification",
            Self::Ringtone => "Ringtone",
            Self::Alarm => "Alarm",
            Self::Call => "Phone",
        }
    }
}

/// Pack a bounded credit into one field, leaving four fields for position.
/// Slot and frame count occupy 16 bits each; byte offset occupies 32 bits.
pub fn pack_credit(slot: u32, frames: u32, offset: u32) -> io::Result<u64> {
    if slot as usize >= BUFFERS || frames == 0 || frames > MAX_FRAMES {
        return Err(invalid("invalid audio credit"));
    }
    Ok(u64::from(slot) | (u64::from(frames) << 16) | (u64::from(offset) << 32))
}

/// Decode credit metadata; the endpoint must also validate format-specific bounds.
pub fn unpack_credit(value: u64) -> io::Result<(u32, u32, u32)> {
    let slot = (value & 0xffff) as u32;
    let frames = ((value >> 16) & 0xffff) as u32;
    let offset = (value >> 32) as u32;
    pack_credit(slot, frames, offset)?;
    Ok((slot, frames, offset))
}

/// Stream direction from Android's perspective.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Direction {
    /// Android produces samples.
    Playback = 1,
    /// Android receives samples.
    Capture = 2,
}

impl TryFrom<u64> for Direction {
    type Error = io::Error;
    fn try_from(value: u64) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Playback),
            2 => Ok(Self::Capture),
            _ => Err(invalid("invalid audio direction")),
        }
    }
}

/// Negotiated, immutable format and allocation bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Format {
    /// Playback or capture.
    pub direction: Direction,
    /// PCM16 little-endian sample rate.
    pub rate: u32,
    /// One or two interleaved channels.
    pub channels: u32,
    /// Allocation capacity in frames, separate from each buffer's valid count.
    pub frames: u32,
}

impl Format {
    /// Validate before allocation or descriptor mapping.
    pub fn validate(self) -> io::Result<Self> {
        if self.rate != RATE
            || !(1..=2).contains(&self.channels)
            || !(128..=MAX_FRAMES).contains(&self.frames)
        {
            return Err(invalid("unsupported PCM16 format or buffer capacity"));
        }
        Ok(self)
    }
    /// Bytes per interleaved frame.
    pub fn frame_bytes(self) -> usize {
        self.channels as usize * 2
    }
    /// Exact descriptor size.
    pub fn bytes(self) -> usize {
        self.frames as usize * self.frame_bytes()
    }
}

/// Versioned little-endian record with request correlation and session generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    /// Operation tag.
    pub op: u32,
    /// Nonzero request identity echoed by the response.
    pub request: u32,
    /// Server-created stream generation; zero only in OPEN.
    pub generation: u64,
    /// Operation-specific fields. Unused fields must be zero.
    pub values: [u64; 5],
}

impl Record {
    /// Encode a complete bounded control packet.
    pub fn encode(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0; RECORD_BYTES];
        bytes[..8].copy_from_slice(b"DLAUD001");
        bytes[8..12].copy_from_slice(&self.op.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.request.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.generation.to_le_bytes());
        for (index, value) in self.values.iter().enumerate() {
            bytes[24 + index * 8..32 + index * 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
    /// Reject malformed records and unknown versions before interpreting fields.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() != RECORD_BYTES || &bytes[..8] != b"DLAUD001" {
            return Err(invalid("invalid audio control record"));
        }
        let op = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let request = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        if !(OPEN..=SUBMIT_POSITION).contains(&op) || request == 0 {
            return Err(invalid("unknown operation or zero request"));
        }
        let generation = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let mut values = [0; 5];
        for (index, value) in values.iter_mut().enumerate() {
            *value = u64::from_le_bytes(bytes[24 + index * 8..32 + index * 8].try_into().unwrap());
        }
        Ok(Self {
            op,
            request,
            generation,
            values,
        })
    }
    /// The role OPEN names: media when it names none.
    pub fn role(self) -> io::Result<Role> {
        let features = self.values[4];
        if features & STREAM_ROLE == 0 {
            return match features & ROLE_MASK {
                0 => Ok(Role::Media),
                _ => Err(invalid("audio role without the role feature")),
            };
        }
        Role::from_wire((features & ROLE_MASK) >> ROLE_SHIFT)
            .ok_or_else(|| invalid("unknown audio role"))
    }
    /// Parse the opening format without narrowing untrusted integers.
    pub fn format(self) -> io::Result<Format> {
        if self.op != OPEN
            || self.generation != 0
            || self.values[4] & !(BURST_POSITION | STREAM_ROLE | ROLE_MASK) != 0
        {
            return Err(invalid("invalid audio open request"));
        }
        Format {
            direction: Direction::try_from(self.values[0])?,
            rate: u32::try_from(self.values[1]).map_err(|_| invalid("rate overflow"))?,
            channels: u32::try_from(self.values[2]).map_err(|_| invalid("channel overflow"))?,
            frames: u32::try_from(self.values[3]).map_err(|_| invalid("frame overflow"))?,
        }
        .validate()
    }
}

/// Dedicated sample mapping. The kernel forbids resizing throughout its lifetime.
pub struct SampleBuffer {
    fd: OwnedFd,
    pointer: NonNull<u8>,
    length: usize,
    writable: bool,
}

// SAFETY: ownership is transferred as a whole; samples are accessed only by the
// stream owner while it has a buffer credit. No Rust reference is retained.
unsafe impl Send for SampleBuffer {}

impl SampleBuffer {
    /// Allocate a host-owned stream buffer with fixed size and no shared metadata.
    pub fn allocate(format: Format) -> io::Result<Self> {
        format.validate()?;
        // SAFETY: constant nul-terminated name and valid flags.
        let raw = unsafe {
            libc::memfd_create(
                c"droidloom-audio".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: memfd_create returned a fresh owned FD.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: the host retains inode ownership. Delegated capture FDs are
        // read-only; a different cell UID cannot reopen them writable via procfs.
        if unsafe { libc::fchmod(fd.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let length = format.bytes();
        // SAFETY: bounded length fits off_t; FD is owned.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), length as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let seals = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: applies immutable size seals to our fresh allocation.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Self::map(fd, length, true)
    }
    /// Validate and map a peer-supplied dedicated sample allocation.
    pub fn map(fd: OwnedFd, length: usize, writable: bool) -> io::Result<Self> {
        if length == 0 || length > MAX_FRAMES as usize * 4 {
            return Err(invalid("invalid audio mapping size"));
        }
        // Check seals before stat: once shrink/grow are forbidden, the size
        // observed below cannot race a peer's resize before mmap/sample access.
        // SAFETY: reads seals from the live descriptor; other file types fail.
        let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
        let required = libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        if seals < 0 || seals & required != required {
            return Err(invalid("audio allocation lacks immutable size seals"));
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: output storage is correctly sized and FD is live.
        if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat initialized the output.
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_size != length as libc::off_t {
            return Err(invalid(
                "audio allocation is not an exact, sealed-size memfd",
            ));
        }
        // SAFETY: immutable FD size was validated; mapping remains live until Drop.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 },
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            pointer: NonNull::new(pointer.cast()).unwrap(),
            length,
            writable,
        })
    }
    /// Address shared with the native backend. Access requires an exclusive credit.
    pub fn pointer(&self) -> *mut u8 {
        self.pointer.as_ptr()
    }
    /// Exact sample capacity in bytes.
    pub fn len(&self) -> usize {
        self.length
    }
    /// Sample allocations are always nonempty.
    pub fn is_empty(&self) -> bool {
        false
    }
    /// Whether this mapping allows writes.
    pub fn writable(&self) -> bool {
        self.writable
    }
    /// Duplicate a capture descriptor with read-only kernel access.
    pub fn readonly_fd(&self) -> io::Result<OwnedFd> {
        std::fs::File::open(format!("/proc/self/fd/{}", self.fd.as_raw_fd())).map(Into::into)
    }
}

impl AsFd for SampleBuffer {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
impl Drop for SampleBuffer {
    fn drop(&mut self) {
        // SAFETY: pointer/length are exactly the owned mapping from mmap.
        unsafe {
            libc::munmap(self.pointer.as_ptr().cast(), self.length);
        }
    }
}

/// Invalid input error shared by codecs and endpoint state machines.
pub fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Apply bounded socket send/receive deadlines outside the PipeWire callback.
pub fn socket_timeout(socket: &SeqPacket, milliseconds: i64) -> io::Result<()> {
    let timeout = libc::timeval {
        tv_sec: milliseconds / 1000,
        tv_usec: (milliseconds % 1000) * 1000,
    };
    for option in [libc::SO_RCVTIMEO, libc::SO_SNDTIMEO] {
        // SAFETY: timeval has the exact ABI size; socket is borrowed and live.
        if unsafe {
            libc::setsockopt(
                socket.as_fd().as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&raw const timeout).cast(),
                std::mem::size_of_val(&timeout) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn open_names_its_role() {
        let mut open = Record {
            op: OPEN,
            request: 1,
            generation: 0,
            values: [1, 48_000, 2, 1024, BURST_POSITION],
        };
        assert_eq!(open.role().unwrap(), Role::Media);
        for role in Role::ALL {
            open.values[4] = BURST_POSITION | role.features();
            assert!(open.format().is_ok());
            assert_eq!(open.role().unwrap(), role);
        }
        open.values[4] = STREAM_ROLE | (9 << ROLE_SHIFT);
        assert!(open.role().is_err());
        open.values[4] = 3 << ROLE_SHIFT;
        assert!(open.role().is_err());
        open.values[4] = 1 << 20;
        assert!(open.format().is_err());
    }
    #[test]
    fn rejects_wrong_version_lengths_and_format_overflow() {
        let open = Record {
            op: OPEN,
            request: 1,
            generation: 0,
            values: [1, 48_000, 2, 1024, 0],
        };
        assert_eq!(
            Record::decode(&open.encode())
                .unwrap()
                .format()
                .unwrap()
                .bytes(),
            4096
        );
        for length in [0, 63, 65] {
            assert!(Record::decode(&vec![0; length]).is_err());
        }
        let mut invalid = open;
        invalid.values[3] = u64::MAX;
        assert!(invalid.format().is_err());
        invalid = open;
        invalid.generation = 1;
        assert!(invalid.format().is_err());
        let mut bytes = open.encode();
        bytes[7] = b'2';
        assert!(Record::decode(&bytes).is_err());
    }
    #[test]
    fn mappings_enforce_size_and_capture_descriptor_access() {
        let format = Format {
            direction: Direction::Capture,
            rate: RATE,
            channels: 1,
            frames: 1024,
        };
        let host = SampleBuffer::allocate(format).unwrap();
        let capture = host.readonly_fd().unwrap();
        assert!(SampleBuffer::map(capture.try_clone().unwrap(), format.bytes(), true).is_err());
        let capture = SampleBuffer::map(capture, format.bytes(), false).unwrap();
        // SAFETY: both mappings are live; no concurrent access in this test.
        unsafe {
            host.pointer().write(91);
            assert_eq!(capture.pointer().read(), 91);
        }
        // SAFETY: kernel must reject shrinking the sealed allocation.
        assert_eq!(unsafe { libc::ftruncate(host.as_fd().as_raw_fd(), 1) }, -1);
        assert!(SampleBuffer::map(host.as_fd().try_clone_to_owned().unwrap(), 1, false).is_err());
    }
}
