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
mod format;
mod writer;

pub use ascii::render_ascii;
pub use elf::{EM_AARCH64, EM_RISCV, write_elf};
pub use elf_read::{ElfFile, ElfReadError, ElfRela, ElfSection, ElfSymbol, parse_elf, reloc_name};
pub use format::{ElfClass, ObjectFormatInfo, RelocKind};
pub use writer::{BinaryEmitError, BinaryWriter};

use tir::{BlockId, OpInstance};

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
pub type InstructionEncoder = fn(&OpInstance) -> Option<EncodedInst>;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_object() -> ObjectFile {
        ObjectFile {
            sections: vec![ObjSection {
                name: ".text".to_string(),
                kind: SectionKind::Text,
                align: 4,
                data: vec![
                    0x33, 0x85, 0xC5, 0x00, // add a0, a1, a2
                    0x67, 0x80, 0x00, 0x00, // ret
                    0xEF, 0x00, 0x00, 0x00, // jal ra, <reloc>
                ],
                relocs: vec![ObjReloc {
                    offset: 8,
                    symbol: "callee".to_string(),
                    r_type: 17,
                    addend: 0,
                }],
                insn_spans: vec![(0, 4), (4, 4), (8, 4)],
            }],
            symbols: vec![ObjSymbol {
                name: "caller".to_string(),
                section: Some(0),
                value: 0,
                size: 12,
                binding: SymBinding::Global,
                kind: SymKind::Func,
            }],
        }
    }

    fn format_info(class: ElfClass) -> ObjectFormatInfo {
        ObjectFormatInfo {
            elf_machine: EM_RISCV,
            elf_class: class,
            elf_flags: 0,
            reloc_for: |_| None,
            pc_rel_scale: |_| 0,
            reloc_field_offset: |_| 0,
        }
    }

    fn roundtrip(class: ElfClass) -> ElfFile {
        let obj = sample_object();
        let bytes = write_elf(&obj, &format_info(class));
        parse_elf(&bytes).expect("emitted ELF parses back")
    }

    fn check_roundtrip(class: ElfClass) {
        let elf = roundtrip(class);
        assert_eq!(elf.class, class);
        assert_eq!(elf.machine, EM_RISCV);
        assert_eq!(elf.etype, 1, "ET_REL");
        assert_eq!(elf.flags, 0);

        let names: Vec<&str> = elf.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["", ".text", ".rela.text", ".symtab", ".strtab", ".shstrtab"]
        );

        let text = &elf.sections[1];
        assert_eq!(text.data, sample_object().sections[0].data);
        assert_eq!(text.flags, 0x6, "SHF_ALLOC | SHF_EXECINSTR");
        assert_eq!(text.addralign, 4);

        // Defined function symbol plus the synthesized undefined reloc target.
        let caller = elf.symbols.iter().find(|s| s.name == "caller").unwrap();
        assert_eq!(caller.section.as_deref(), Some(".text"));
        assert_eq!(caller.value, 0);
        assert_eq!(caller.size, 12);
        assert_eq!(caller.binding, 1, "STB_GLOBAL");
        assert_eq!(caller.sym_type, 2, "STT_FUNC");

        let callee = elf.symbols.iter().find(|s| s.name == "callee").unwrap();
        assert_eq!(callee.section, None);
        assert_eq!(callee.binding, 1, "STB_GLOBAL");
        assert_eq!(callee.sym_type, 0, "STT_NOTYPE");

        assert_eq!(
            elf.relocations,
            vec![ElfRela {
                section: ".text".to_string(),
                offset: 8,
                symbol: "callee".to_string(),
                r_type: 17,
                addend: 0,
            }]
        );
    }

    #[test]
    fn elf64_roundtrip() {
        check_roundtrip(ElfClass::Elf64);
    }

    #[test]
    fn elf32_roundtrip() {
        check_roundtrip(ElfClass::Elf32);
    }

    #[test]
    fn ascii_rendering_is_stable() {
        let rendered = render_ascii(&sample_object());
        assert_eq!(
            rendered,
            ".section .text\n\
             caller:\n\
             \x20 [0x33, 0x85, 0xC5, 0x00]\n\
             \x20 [0x67, 0x80, 0x00, 0x00]\n\
             \x20 [0xEF, 0x00, 0x00, 0x00]\n"
        );
    }

    #[test]
    fn parse_rejects_non_elf() {
        assert_eq!(parse_elf(b"not an elf"), Err(ElfReadError::NotAnElf));
    }
}
