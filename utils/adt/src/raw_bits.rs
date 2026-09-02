use crate::{APFloat, APInt};

const BYTE_SIZE: usize = 8;

/// Untyped, byte-granular bits backing a value (e.g. a vector register wider than a
/// word, readable as integer or float lanes). Stored little-endian: `storage[0]` is least significant.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RawBits {
    storage: Vec<u8>,
}

impl RawBits {
    /// A zeroed value of `n` bits. `n` must be a whole number of bytes.
    pub fn new(n: usize) -> Self {
        assert!(
            n.is_multiple_of(BYTE_SIZE),
            "RawBits width must be byte-aligned"
        );
        RawBits {
            storage: vec![0; n / BYTE_SIZE],
        }
    }

    /// Wrap raw little-endian bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        RawBits { storage: bytes }
    }

    /// The little-endian bytes backing this value.
    pub fn bytes(&self) -> &[u8] {
        &self.storage
    }

    /// The width in bits (always a multiple of 8).
    pub fn width(&self) -> usize {
        self.storage.len() * BYTE_SIZE
    }

    /// Truncate or zero-extend to `bits` (a whole number of bytes); the low bits
    /// are preserved. Used to normalize a stored value to a register class's
    /// width — e.g. reading `v0` (128) after a `d0` (64) write zero-extends.
    pub fn resized(&self, bits: usize) -> Self {
        assert!(
            bits.is_multiple_of(BYTE_SIZE),
            "RawBits width must be byte-aligned"
        );
        let mut storage = self.storage.clone();
        storage.resize(bits / BYTE_SIZE, 0);
        RawBits { storage }
    }

    /// Reinterpret an integer as raw bits, widened to a whole number of bytes.
    pub fn from_apint(value: &APInt) -> Self {
        let num_bytes = value.width().div_ceil(BYTE_SIZE as u32) as usize;
        let raw = value.to_u64();
        let storage = (0..num_bytes)
            .map(|i| (raw >> (i * BYTE_SIZE)) as u8)
            .collect();
        RawBits { storage }
    }

    /// Reinterpret these bits as an unsigned integer of the same width (must fit a word).
    pub fn to_apint(&self) -> APInt {
        assert!(
            self.width() <= 64,
            "RawBits wider than 64 bits cannot be read as a single integer"
        );
        let mut value = 0u64;
        for (i, byte) in self.storage.iter().enumerate() {
            value |= u64::from(*byte) << (i * BYTE_SIZE);
        }
        APInt::new(self.width() as u32, value)
    }

    /// Reinterpret a float as raw bits.
    pub fn from_apfloat(value: &APFloat) -> Self {
        let num_bytes = value.bit_width().div_ceil(BYTE_SIZE as u32) as usize;
        let raw = value.to_bits();
        let storage = (0..num_bytes)
            .map(|i| (raw >> (i * BYTE_SIZE)) as u8)
            .collect();
        RawBits { storage }
    }

    /// Split into `lanes` equal byte-aligned pieces, lane 0 from the low bits.
    pub fn split(&self, lanes: usize) -> Vec<RawBits> {
        assert!(lanes > 0, "RawBits split requires a positive lane count");
        assert!(
            self.storage.len().is_multiple_of(lanes),
            "RawBits of {} bits does not split into {lanes} byte-aligned lanes",
            self.width()
        );
        let lane_bytes = self.storage.len() / lanes;
        self.storage
            .chunks(lane_bytes)
            .map(|chunk| RawBits {
                storage: chunk.to_vec(),
            })
            .collect()
    }

    /// Split into `lanes` byte-aligned pieces of `lane_bits` each, lane 0 from
    /// the low bits. Bits beyond `lanes * lane_bits` (a register wider than the
    /// active element group) are ignored; missing high bits read as zero — a
    /// stored value is the low bits of a conceptually wider register.
    pub fn split_lanes(&self, lanes: usize, lane_bits: usize) -> Vec<RawBits> {
        assert!(lanes > 0, "RawBits split requires a positive lane count");
        assert!(
            lane_bits > 0 && lane_bits.is_multiple_of(BYTE_SIZE),
            "RawBits lanes must be a positive whole number of bytes, got {lane_bits} bits"
        );
        let lane_bytes = lane_bits / BYTE_SIZE;
        let mut storage = self.storage.clone();
        storage.resize(lanes * lane_bytes, 0);
        storage
            .chunks(lane_bytes)
            .take(lanes)
            .map(|chunk| RawBits {
                storage: chunk.to_vec(),
            })
            .collect()
    }

    /// Concatenate lanes, lane 0 in the low bits; inverse of [`RawBits::split`].
    pub fn concat(lanes: &[RawBits]) -> RawBits {
        let storage = lanes
            .iter()
            .flat_map(|lane| lane.storage.iter().copied())
            .collect();
        RawBits { storage }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_concat_are_inverse() {
        let raw = RawBits::from_bytes(vec![0x01, 0x02, 0x03, 0x04]);
        let lanes = raw.split(4);
        assert_eq!(lanes.len(), 4);
        assert_eq!(lanes[0].bytes(), &[0x01]);
        assert_eq!(lanes[3].bytes(), &[0x04]);
        assert_eq!(RawBits::concat(&lanes), raw);
    }

    #[test]
    fn integer_reinterpretation_roundtrips() {
        let value = APInt::new(32, 0xDEAD_BEEF);
        let raw = RawBits::from_apint(&value);
        assert_eq!(raw.width(), 32);
        assert_eq!(raw.to_apint(), value);
    }
}
