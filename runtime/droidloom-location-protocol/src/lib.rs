//! Location between Android's GNSS HAL and the host user's location broker.
//!
//! Records are exactly [`RECORD_BYTES`] bytes on a sequenced-packet socket,
//! little-endian, starting with [`MAGIC`]. Android sends [`START`] with its
//! wanted interval and [`STOP`]; the host answers with [`FIX`] records while
//! started, and [`DENIED`] when the host refuses location (its privacy switch
//! is off, or no location service runs). Closing the socket stops.

use std::io;

/// Fixed record size.
pub const RECORD_BYTES: usize = 64;
/// First bytes of every record.
pub const MAGIC: [u8; 8] = *b"DLLOC001";

/// Android → host: start, `interval_ms` between fixes.
pub const START: u32 = 1;
/// Android → host: stop.
pub const STOP: u32 = 2;
/// Host → Android: one fix.
pub const FIX: u32 = 3;
/// Host → Android: location is refused or unavailable.
pub const DENIED: u32 = 4;

/// What a [`FIX`] knows besides latitude, longitude and accuracy.
pub const HAS_ALTITUDE: u32 = 1 << 0;
/// The fix has a speed.
pub const HAS_SPEED: u32 = 1 << 1;
/// The fix has a heading.
pub const HAS_BEARING: u32 = 1 << 2;

/// One location fix.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fix {
    /// `HAS_*` flags.
    pub flags: u32,
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
    /// Metres above the WGS84 ellipsoid, with `HAS_ALTITUDE`.
    pub altitude: f64,
    /// Horizontal accuracy, metres.
    pub accuracy: f32,
    /// Metres per second, with `HAS_SPEED`.
    pub speed: f32,
    /// Degrees clockwise from north, with `HAS_BEARING`.
    pub bearing: f32,
    /// When the fix was taken, milliseconds since the Unix epoch.
    pub time_ms: i64,
}

/// A decoded record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Record {
    /// Start, with the interval in milliseconds.
    Start(u32),
    /// Stop.
    Stop,
    /// A fix.
    Fix(Fix),
    /// Location is refused or unavailable.
    Denied,
}

fn put(bytes: &mut [u8; RECORD_BYTES], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

fn take<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    let mut out = [0_u8; N];
    out.copy_from_slice(&bytes[at..at + N]);
    out
}

impl Record {
    /// The record's bytes.
    pub fn encode(self) -> [u8; RECORD_BYTES] {
        let mut bytes = [0_u8; RECORD_BYTES];
        put(&mut bytes, 0, &MAGIC);
        match self {
            Record::Start(interval_ms) => {
                put(&mut bytes, 8, &START.to_le_bytes());
                put(&mut bytes, 12, &interval_ms.to_le_bytes());
            }
            Record::Stop => put(&mut bytes, 8, &STOP.to_le_bytes()),
            Record::Denied => put(&mut bytes, 8, &DENIED.to_le_bytes()),
            Record::Fix(fix) => {
                put(&mut bytes, 8, &FIX.to_le_bytes());
                put(&mut bytes, 12, &fix.flags.to_le_bytes());
                put(&mut bytes, 16, &fix.latitude.to_le_bytes());
                put(&mut bytes, 24, &fix.longitude.to_le_bytes());
                put(&mut bytes, 32, &fix.altitude.to_le_bytes());
                put(&mut bytes, 40, &fix.accuracy.to_le_bytes());
                put(&mut bytes, 44, &fix.speed.to_le_bytes());
                put(&mut bytes, 48, &fix.bearing.to_le_bytes());
                put(&mut bytes, 52, &fix.time_ms.to_le_bytes());
            }
        }
        bytes
    }

    /// A record from its bytes.
    ///
    /// # Errors
    ///
    /// A record of the wrong size, without the magic, of an unknown kind, or
    /// a fix whose numbers are out of range.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        if bytes.len() != RECORD_BYTES || bytes[..8] != MAGIC {
            return Err(invalid("not a location record"));
        }
        let word = |at| u32::from_le_bytes(take(bytes, at));
        match word(8) {
            START => Ok(Record::Start(word(12))),
            STOP => Ok(Record::Stop),
            DENIED => Ok(Record::Denied),
            FIX => {
                let fix = Fix {
                    flags: word(12),
                    latitude: f64::from_le_bytes(take(bytes, 16)),
                    longitude: f64::from_le_bytes(take(bytes, 24)),
                    altitude: f64::from_le_bytes(take(bytes, 32)),
                    accuracy: f32::from_le_bytes(take(bytes, 40)),
                    speed: f32::from_le_bytes(take(bytes, 44)),
                    bearing: f32::from_le_bytes(take(bytes, 48)),
                    time_ms: i64::from_le_bytes(take(bytes, 52)),
                };
                if !(-90.0..=90.0).contains(&fix.latitude)
                    || !(-180.0..=180.0).contains(&fix.longitude)
                    || !fix.accuracy.is_finite()
                    || fix.accuracy < 0.0
                {
                    return Err(invalid("fix out of range"));
                }
                Ok(Record::Fix(fix))
            }
            _ => Err(invalid("unknown location record")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fix, HAS_SPEED, RECORD_BYTES, Record};

    #[test]
    fn records_round_trip() {
        let fix = Fix {
            flags: HAS_SPEED,
            latitude: 40.4,
            longitude: -3.7,
            altitude: 0.0,
            accuracy: 4.5,
            speed: 1.25,
            bearing: 0.0,
            time_ms: 1_794_000_000_000,
        };
        for record in [Record::Start(1000), Record::Stop, Record::Denied, Record::Fix(fix)] {
            assert_eq!(Record::decode(&record.encode()).unwrap(), record);
        }
    }

    #[test]
    fn bad_records_are_refused() {
        assert!(Record::decode(&[0; RECORD_BYTES]).is_err());
        assert!(Record::decode(&Record::Stop.encode()[..10]).is_err());
        let mut far = Record::Fix(Fix { latitude: 91.0, ..Fix::default() }).encode();
        assert!(Record::decode(&far).is_err());
        far[8] = 9;
        assert!(Record::decode(&far).is_err());
    }
}
