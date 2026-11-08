//! Sensors between Android's sensors HAL and the host user's sensor broker.
//!
//! Records are exactly [`RECORD_BYTES`] bytes on a sequenced-packet socket,
//! little-endian, starting with [`MAGIC`]. Android sends [`WANT`] with the
//! set of host sensors its listeners need, again whenever the set changes;
//! the host answers with a [`SAMPLE`] for each new reading of a wanted
//! sensor, starting with the current one. Closing the socket wants none.

use std::io;

/// Fixed record size.
pub const RECORD_BYTES: usize = 64;
/// First bytes of every record.
pub const MAGIC: [u8; 8] = *b"DLSNS001";

/// Android → host: the sensors wanted, as `Sensor` bits.
pub const WANT: u32 = 1;
/// Host → Android: one reading.
pub const SAMPLE: u32 = 2;

/// A host sensor, as one bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Sensor {
    /// m/s²: x toward the right edge, y toward the top edge, z out of the
    /// screen; about +9.8 up at rest.
    Accel = 1 << 0,
    /// rad/s about the same axes, counterclockwise looking down each.
    Gyro = 1 << 1,
    /// Lux, in `x`.
    Light = 1 << 2,
    /// 1 in `x` when something is close to the screen, else 0.
    Proximity = 1 << 3,
    /// Degrees clockwise from magnetic north in `x`, as the host's compass
    /// gives it (the top edge while the phone lies nearer flat, the back
    /// once it stands nearer upright), or -1 before the first reading.
    Heading = 1 << 4,
}

impl Sensor {
    /// Every sensor.
    pub const ALL: [Self; 5] = [
        Self::Accel,
        Self::Gyro,
        Self::Light,
        Self::Proximity,
        Self::Heading,
    ];

    /// Its bit.
    pub const fn bit(self) -> u32 {
        self as u32
    }

    fn from_bit(bit: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|sensor| sensor.bit() == bit)
    }
}

/// One reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// Which sensor read it.
    pub sensor: Sensor,
    /// The reading, as its sensor says; unused axes are 0.
    pub x: f32,
    /// See `x`.
    pub y: f32,
    /// See `x`.
    pub z: f32,
    /// 0 unreliable, 1 low, 2 medium, 3 high.
    pub accuracy: u32,
}

/// A decoded record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Record {
    /// The sensors wanted, as bits.
    Want(u32),
    /// A reading.
    Sample(Sample),
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
            Record::Want(sensors) => {
                put(&mut bytes, 8, &WANT.to_le_bytes());
                put(&mut bytes, 12, &sensors.to_le_bytes());
            }
            Record::Sample(sample) => {
                put(&mut bytes, 8, &SAMPLE.to_le_bytes());
                put(&mut bytes, 12, &sample.sensor.bit().to_le_bytes());
                put(&mut bytes, 16, &sample.x.to_le_bytes());
                put(&mut bytes, 20, &sample.y.to_le_bytes());
                put(&mut bytes, 24, &sample.z.to_le_bytes());
                put(&mut bytes, 28, &sample.accuracy.to_le_bytes());
            }
        }
        bytes
    }

    /// A record from its bytes.
    ///
    /// # Errors
    ///
    /// A record of the wrong size, without the magic, of an unknown kind, or
    /// a sample of an unknown sensor or with a value that is not finite.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        if bytes.len() != RECORD_BYTES || bytes[..8] != MAGIC {
            return Err(invalid("not a sensor record"));
        }
        let word = |at| u32::from_le_bytes(take(bytes, at));
        let float = |at| f32::from_le_bytes(take(bytes, at));
        match word(8) {
            WANT => Ok(Record::Want(word(12))),
            SAMPLE => {
                let sample = Sample {
                    sensor: Sensor::from_bit(word(12))
                        .ok_or_else(|| invalid("unknown sensor"))?,
                    x: float(16),
                    y: float(20),
                    z: float(24),
                    accuracy: word(28).min(3),
                };
                if ![sample.x, sample.y, sample.z].iter().all(|value| value.is_finite()) {
                    return Err(invalid("sample out of range"));
                }
                Ok(Record::Sample(sample))
            }
            _ => Err(invalid("unknown sensor record")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RECORD_BYTES, Record, Sample, Sensor};

    #[test]
    fn records_round_trip() {
        let sample = Sample {
            sensor: Sensor::Accel,
            x: 0.25,
            y: 9.75,
            z: -0.5,
            accuracy: 3,
        };
        for record in [Record::Want(0b10101), Record::Sample(sample)] {
            assert_eq!(Record::decode(&record.encode()).unwrap(), record);
        }
    }

    #[test]
    fn bad_records_are_refused() {
        assert!(Record::decode(&[0; RECORD_BYTES]).is_err());
        assert!(Record::decode(&Record::Want(1).encode()[..10]).is_err());
        let sample = Sample {
            sensor: Sensor::Light,
            x: f32::NAN,
            y: 0.0,
            z: 0.0,
            accuracy: 3,
        };
        assert!(Record::decode(&Record::Sample(sample).encode()).is_err());
        let mut unknown = Record::Want(1).encode();
        unknown[8] = 9;
        assert!(Record::decode(&unknown).is_err());
        let mut sensor = Record::Sample(Sample { x: 1.0, ..sample }).encode();
        sensor[12] = 3;
        assert!(Record::decode(&sensor).is_err());
    }
}
