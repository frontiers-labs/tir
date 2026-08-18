//! Format-neutral building blocks for object-file emission.
//!
//! TMDL-generated encoders turn a machine instruction into bytes plus a list
//! of fixups for operands whose value is not known at encode time (branch
//! targets, external symbols). Patchers re-scatter a resolved value into the
//! instruction's immediate bits once layout is known. The laid-out result is
//! an [`ObjectFile`], which a format backend (ELF today) serializes to bytes.

mod ascii;
mod elf;
mod elf_read;
mod encodings;
mod format;
mod writer;

pub use ascii::render_ascii;
pub use elf::{EM_AARCH64, EM_RISCV, EM_X86_64, write_elf};
pub use elf_read::{ElfFile, ElfReadError, ElfRela, ElfSection, ElfSymbol, parse_elf, reloc_name};
pub use encodings::{
    DecodeField, DecodeFieldKind, DecodeSpec, EncodeField, EncodeSpec, FieldRun, PatchSpec,
    decode_with, encode_with, patch_with,
};
pub use format::{ElfClass, ObjectFormatInfo, RelocKind};
pub use writer::{BinaryEmitError, BinaryWriter, ObjectEmission};

use tir::{BlockId, OpHandle};

/// What an unresolved instruction operand points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixupTarget {
    /// A basic block in the same symbol; resolved to a pc-relative delta
    /// during layout.
    Block(BlockId),
    /// A named symbol; becomes a relocation if it cannot be resolved locally.
    Symbol(String),
}

/// An operand left as zero bits in the encoded instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstFixup {
    /// TMDL operand name the fixup applies to (e.g. `"imm"`).
    pub operand: &'static str,
    pub target: FixupTarget,
}

/// One encoded machine instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedInst {
    /// Little-endian instruction bytes; fixup bits are zero.
    pub bytes: Vec<u8>,
    pub fixups: Vec<InstFixup>,
}

/// Encodes one operation. `None` means the operation cannot be encoded
/// (e.g. a virtual register survived register allocation).
pub type InstructionEncoder = fn(&OpHandle) -> Option<EncodedInst>;

/// Scatters a resolved fixup value into the instruction bytes. `None` means
/// the value does not fit the operand's encoding (out of range or misaligned).
pub type InstructionPatcher = fn(&mut [u8], i64) -> Option<()>;

/// A relocatable object in format-neutral form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectFile {
    pub sections: Vec<ObjSection>,
    pub symbols: Vec<ObjSymbol>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Text,
    ReadOnlyData,
    Data,
    UninitializedData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjSection {
    pub name: String,
    pub kind: SectionKind,
    pub align: u64,
    pub data: Vec<u8>,
    pub relocs: Vec<ObjReloc>,
    /// `(offset, length)` of each encoded instruction, in layout order.
    /// Only consumed by the ASCII rendering used in lit tests.
    pub insn_spans: Vec<(u64, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjReloc {
    /// Byte offset of the fixed-up instruction within the section.
    pub offset: u64,
    pub symbol: String,
    /// Format- and target-specific relocation type (e.g. an ELF `r_type`).
    pub r_type: u32,
    pub addend: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymBinding {
    Local,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    NoType,
    Func,
    Object,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjSymbol {
    pub name: String,
    /// Index into [`ObjectFile::sections`]; `None` for undefined symbols.
    pub section: Option<usize>,
    pub value: u64,
    pub size: u64,
    pub binding: SymBinding,
    pub kind: SymKind,
}
