use crate::BuildError;

/// A UTF-8 byte count or offset.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextSize(u32);

impl TextSize {
    /// Zero bytes.
    pub const ZERO: Self = Self(0);
    /// Creates an offset from a representable byte count.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
    /// Returns the byte count.
    pub const fn get(self) -> u32 {
        self.0
    }
    /// Converts a platform byte count without truncation.
    pub fn from_usize(value: usize) -> Result<Self, BuildError> {
        u32::try_from(value)
            .map(Self)
            .map_err(|_| BuildError::Overflow)
    }
    /// Adds byte counts without wrapping.
    pub fn checked_add(self, other: Self) -> Result<Self, BuildError> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or(BuildError::Overflow)
    }
}

/// A half-open UTF-8 byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRange {
    start: TextSize,
    end: TextSize,
}
impl TextRange {
    /// Rejects reversed ranges.
    pub fn new(start: TextSize, end: TextSize) -> Result<Self, BuildError> {
        if start > end {
            return Err(BuildError::InvalidRange);
        }
        Ok(Self { start, end })
    }
    pub(crate) fn at(start: TextSize, len: TextSize) -> Self {
        Self {
            start,
            end: TextSize(start.0 + len.0),
        }
    }
    /// First byte offset.
    pub const fn start(self) -> TextSize {
        self.start
    }
    /// Exclusive end offset.
    pub const fn end(self) -> TextSize {
        self.end
    }
    /// Number of bytes in the range.
    pub const fn len(self) -> TextSize {
        TextSize(self.end.0 - self.start.0)
    }
    /// Whether the range contains no bytes.
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}
