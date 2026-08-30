//! Format-neutral building blocks for object-file emission.
//!
//! An instruction's TMDL-generated [`EncodeSpec`] turns it into bytes plus a
//! list of fixups for operands whose value is not known at encode time (branch
//! targets, external symbols); its [`PatchSpec`] re-scatters a resolved value
//! into the immediate bits once layout is known. Both are fields of the
//! instruction's [`crate::backend::InstrInfo`]. The laid-out result is
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

use tir::BlockId;

/// What an unresolved instruction operand points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixupTarget {
    /// A basic block in the same symbol; resolved to a pc-relative delta
    /// during layout.
    Block(BlockId),
    /// A named symbol; becomes a relocation if it cannot be resolved locally.
    Symbol(String),
}

/// One encoded machine instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedInst {
    /// Little-endian instruction bytes; fixup bits are zero.
    pub bytes: Vec<u8>,
    /// Operands left as zero bits, to be resolved at layout time.
    pub fixups: Vec<FixupTarget>,
}

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
