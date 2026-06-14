use crate::utils::{APFloat, APInt};

const BYTE_SIZE: usize = 8;

/// An untyped, byte-granular sequence of bits — the raw contents of a value
/// before it is interpreted as an integer or a float. Vector registers are
/// represented this way: they can be wider than a machine word (so they do not
/// fit an [`APInt`]) and the same bits may be read as integer or floating-point
/// lanes. Bytes are stored little-endian: `storage[0]` is the least significant.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// Reinterpret an integer as raw bits, widened to a whole number of bytes.
    pub fn from_apint(value: &APInt) -> Self {
        let num_bytes = value.width().div_ceil(BYTE_SIZE as u32) as usize;
        let raw = value.to_u64();
        let storage = (0..num_bytes)
            .map(|i| (raw >> (i * BYTE_SIZE)) as u8)
            .collect();
        RawBits { storage }
    }

    /// Reinterpret these bits as an unsigned integer of the same width. The width
    /// must fit a machine word, which holds for individual lanes.
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

    /// Reinterpret these bits as a float of the given IEEE-style format.
    pub fn to_apfloat(
        &self,
        exp_width: u32,
        mant_width: u32,
        explicit_leading_bit: bool,
    ) -> APFloat {
        let mut bits = 0u128;
        for (i, byte) in self.storage.iter().enumerate() {
            bits |= u128::from(*byte) << (i * BYTE_SIZE);
        }
        APFloat::from_bits(exp_width, mant_width, explicit_leading_bit, bits)
    }

    /// Split into `lanes` equal-width pieces, lane 0 taken from the low bits. The
    /// width must divide evenly into byte-aligned lanes.
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

    /// Concatenate lanes into one value, lane 0 in the low bits. The inverse of
    /// [`RawBits::split`].
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

    #[test]
    fn float_reinterpretation_roundtrips() {
        // The same bits a lane holds can be read as a float: a vector is not
        // committed to an integer interpretation.
        let value = APFloat::from_f32(1.5);
        let raw = RawBits::from_apfloat(&value);
        assert_eq!(raw.width(), 32);
        let back = raw.to_apfloat(8, 23, false);
        assert_eq!(back.to_f32(), 1.5);
    }
}
