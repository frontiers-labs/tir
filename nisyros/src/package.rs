//! Bounded, little-endian encoding for Vulkan pipeline-binary packages.

const MAGIC: &[u8; 8] = b"NISYROS\0";
const PACKAGE_VERSION: u32 = 1;
const EXECUTABLE_ABI_VERSION: u32 = 1;
const VULKAN_PIPELINE_BINARY_FORMAT: u32 = 1;
const HEADER_SIZE: usize = 40;
const MAX_PACKAGE_SIZE: usize = 128 * 1024 * 1024;
const MAX_BINARY_SIZE: usize = 64 * 1024 * 1024;
const MAX_BINARY_COUNT: usize = 16;
const MAX_ENTRY_LEN: usize = 128;
const MAX_ARGUMENT_SIZE: u32 = 64 * 1024;
const MAX_ARGUMENT_FIELDS: usize = 256;

pub const ARG_DEVICE_ADDRESS: u32 = 1;
pub const ARG_VALUE: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BinaryKey {
    pub size: u32,
    pub bytes: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArgumentField {
    pub offset: u32,
    pub width: u32,
    pub kind: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineBinary {
    pub key: BinaryKey,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub global_key: BinaryKey,
    pub vendor_id: u32,
    pub device_id: u32,
    pub api_version: u32,
    pub entry: String,
    pub argument_size: u32,
    pub argument_alignment: u32,
    pub argument_fields: Vec<ArgumentField>,
    pub workgroup: [u32; 3],
    pub required_features: u64,
    pub scratch_bytes: u64,
    pub binaries: Vec<PipelineBinary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageError {
    Truncated,
    Invalid,
    Unsupported,
}

impl Package {
    pub fn parse(bytes: &[u8]) -> Result<Self, PackageError> {
        if bytes.len() < HEADER_SIZE {
            return Err(PackageError::Truncated);
        }
        if bytes.len() > MAX_PACKAGE_SIZE {
            return Err(PackageError::Invalid);
        }
        let mut reader = Reader::new(bytes);
        if reader.take(8)? != MAGIC {
            return Err(PackageError::Invalid);
        }
        if reader.u32()? != PACKAGE_VERSION
            || reader.u32()? != EXECUTABLE_ABI_VERSION
            || reader.u32()? != VULKAN_PIPELINE_BINARY_FORMAT
        {
            return Err(PackageError::Unsupported);
        }
        if reader.u32()? != 0 || reader.u64()? != bytes.len() as u64 {
            return Err(PackageError::Invalid);
        }
        let checksum = reader.u64()?;
        if checksum != checksum64(&bytes[HEADER_SIZE..]) {
            return Err(PackageError::Invalid);
        }
        let global_key = reader.key()?;
        let vendor_id = reader.u32()?;
        let device_id = reader.u32()?;
        let api_version = reader.u32()?;
        let entry_len = reader.u32()? as usize;
        if entry_len == 0 || entry_len > MAX_ENTRY_LEN {
            return Err(PackageError::Invalid);
        }
        let entry = std::str::from_utf8(reader.take(entry_len)?)
            .map_err(|_| PackageError::Invalid)?
            .to_owned();
        let argument_size = reader.u32()?;
        let argument_alignment = reader.u32()?;
        let field_count = reader.u32()? as usize;
        if field_count > MAX_ARGUMENT_FIELDS {
            return Err(PackageError::Invalid);
        }
        let mut argument_fields = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            argument_fields.push(ArgumentField {
                offset: reader.u32()?,
                width: reader.u32()?,
                kind: reader.u32()?,
            });
        }
        let workgroup = [reader.u32()?, reader.u32()?, reader.u32()?];
        let required_features = reader.u64()?;
        let scratch_bytes = reader.u64()?;
        let binary_count = reader.u32()? as usize;
        if binary_count == 0 || binary_count > MAX_BINARY_COUNT {
            return Err(PackageError::Invalid);
        }
        let mut binaries = Vec::with_capacity(binary_count);
        for _ in 0..binary_count {
            let key = reader.key()?;
            let data_len = usize::try_from(reader.u64()?).map_err(|_| PackageError::Invalid)?;
            if data_len == 0 || data_len > MAX_BINARY_SIZE {
                return Err(PackageError::Invalid);
            }
            binaries.push(PipelineBinary {
                key,
                data: reader.take(data_len)?.to_vec(),
            });
        }
        if reader.remaining() != 0 {
            return Err(PackageError::Invalid);
        }
        let package = Self {
            global_key,
            vendor_id,
            device_id,
            api_version,
            entry,
            argument_size,
            argument_alignment,
            argument_fields,
            workgroup,
            required_features,
            scratch_bytes,
            binaries,
        };
        package.validate()?;
        Ok(package)
    }

    pub fn encode(&self) -> Result<Vec<u8>, PackageError> {
        self.validate()?;
        let mut output = Vec::with_capacity(HEADER_SIZE);
        output.extend_from_slice(MAGIC);
        write_u32(&mut output, PACKAGE_VERSION);
        write_u32(&mut output, EXECUTABLE_ABI_VERSION);
        write_u32(&mut output, VULKAN_PIPELINE_BINARY_FORMAT);
        write_u32(&mut output, 0);
        write_u64(&mut output, 0);
        write_u64(&mut output, 0);
        write_key(&mut output, self.global_key);
        write_u32(&mut output, self.vendor_id);
        write_u32(&mut output, self.device_id);
        write_u32(&mut output, self.api_version);
        write_u32(&mut output, self.entry.len() as u32);
        output.extend_from_slice(self.entry.as_bytes());
        write_u32(&mut output, self.argument_size);
        write_u32(&mut output, self.argument_alignment);
        write_u32(&mut output, self.argument_fields.len() as u32);
        for field in &self.argument_fields {
            write_u32(&mut output, field.offset);
            write_u32(&mut output, field.width);
            write_u32(&mut output, field.kind);
        }
        for dimension in self.workgroup {
            write_u32(&mut output, dimension);
        }
        write_u64(&mut output, self.required_features);
        write_u64(&mut output, self.scratch_bytes);
        write_u32(&mut output, self.binaries.len() as u32);
        for binary in &self.binaries {
            write_key(&mut output, binary.key);
            write_u64(&mut output, binary.data.len() as u64);
            output.extend_from_slice(&binary.data);
        }
        if output.len() > MAX_PACKAGE_SIZE {
            return Err(PackageError::Invalid);
        }
        let total_size = output.len() as u64;
        output[24..32].copy_from_slice(&total_size.to_le_bytes());
        let checksum = checksum64(&output[HEADER_SIZE..]);
        output[32..40].copy_from_slice(&checksum.to_le_bytes());
        Ok(output)
    }

    fn validate(&self) -> Result<(), PackageError> {
        if self.global_key.size == 0
            || self.global_key.size > 32
            || self.entry.is_empty()
            || self.entry.len() > MAX_ENTRY_LEN
            || self.entry.as_bytes().contains(&0)
            || self.argument_size == 0
            || self.argument_size > MAX_ARGUMENT_SIZE
            || !self.argument_alignment.is_power_of_two()
            || self.argument_alignment > 256
            || self.argument_fields.is_empty()
            || self.argument_fields.len() > MAX_ARGUMENT_FIELDS
            || self.workgroup.contains(&0)
            || self.binaries.is_empty()
            || self.binaries.len() > MAX_BINARY_COUNT
        {
            return Err(PackageError::Invalid);
        }
        let mut previous_end = 0;
        for field in &self.argument_fields {
            let Some(end) = field.offset.checked_add(field.width) else {
                return Err(PackageError::Invalid);
            };
            if field.width == 0 || field.offset < previous_end || end > self.argument_size {
                return Err(PackageError::Invalid);
            }
            if (field.kind == ARG_DEVICE_ADDRESS
                && (field.width != 8 || !field.offset.is_multiple_of(8)))
                || (field.kind != ARG_DEVICE_ADDRESS && field.kind != ARG_VALUE)
            {
                return Err(PackageError::Invalid);
            }
            previous_end = end;
        }
        for binary in &self.binaries {
            if binary.key.size == 0
                || binary.key.size > 32
                || binary.data.is_empty()
                || binary.data.len() > MAX_BINARY_SIZE
            {
                return Err(PackageError::Invalid);
            }
        }
        if self.required_features != 0 || self.scratch_bytes != 0 {
            return Err(PackageError::Unsupported);
        }
        Ok(())
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], PackageError> {
        let end = self.offset.checked_add(len).ok_or(PackageError::Invalid)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(PackageError::Truncated)?;
        self.offset = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, PackageError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed width"),
        ))
    }

    fn u64(&mut self) -> Result<u64, PackageError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed width"),
        ))
    }

    fn key(&mut self) -> Result<BinaryKey, PackageError> {
        let size = self.u32()?;
        let bytes: [u8; 32] = self.take(32)?.try_into().expect("fixed width");
        if size == 0 || size > 32 || bytes[size as usize..].iter().any(|&byte| byte != 0) {
            return Err(PackageError::Invalid);
        }
        Ok(BinaryKey { size, bytes })
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }
}

fn write_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_key(output: &mut Vec<u8>, key: BinaryKey) {
    write_u32(output, key.size);
    output.extend_from_slice(&key.bytes);
}

fn checksum64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Package {
        Package {
            global_key: BinaryKey {
                size: 3,
                bytes: [1; 32],
            },
            vendor_id: 0x1002,
            device_id: 0x1586,
            api_version: 1 << 22 | 3 << 12,
            entry: "main".into(),
            argument_size: 24,
            argument_alignment: 8,
            argument_fields: vec![
                ArgumentField {
                    offset: 0,
                    width: 8,
                    kind: ARG_DEVICE_ADDRESS,
                },
                ArgumentField {
                    offset: 8,
                    width: 8,
                    kind: ARG_DEVICE_ADDRESS,
                },
                ArgumentField {
                    offset: 16,
                    width: 4,
                    kind: ARG_VALUE,
                },
            ],
            workgroup: [64, 1, 1],
            required_features: 0,
            scratch_bytes: 0,
            binaries: vec![PipelineBinary {
                key: BinaryKey {
                    size: 2,
                    bytes: [2; 32],
                },
                data: vec![3, 4, 5],
            }],
        }
    }

    #[test]
    fn round_trip_and_reject_corruption() {
        let mut original = sample();
        original.global_key.bytes[3..].fill(0);
        original.binaries[0].key.bytes[2..].fill(0);
        let bytes = original.encode().unwrap();
        assert_eq!(Package::parse(&bytes).unwrap(), original);
        for end in 0..bytes.len() {
            assert!(Package::parse(&bytes[..end]).is_err());
        }
        let mut corrupted = bytes.clone();
        *corrupted.last_mut().unwrap() ^= 1;
        assert_eq!(Package::parse(&corrupted), Err(PackageError::Invalid));
    }

    #[test]
    fn rejects_overlapping_argument_fields() {
        let mut package = sample();
        package.argument_fields[1].offset = 4;
        assert_eq!(package.encode(), Err(PackageError::Invalid));
    }
}
