//! AArch64 ELF format parameters and relocation selection.

use tir::backend::binary::{EM_AARCH64, ElfClass, ObjectFormatInfo, RelocKind};

const R_AARCH64_ADR_PREL_LO21: u32 = 274;
const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_ABS32: u32 = 258;
const R_AARCH64_TSTBR14: u32 = 279;
const R_AARCH64_CONDBR19: u32 = 280;
const R_AARCH64_JUMP26: u32 = 282;
const R_AARCH64_CALL26: u32 = 283;

pub(crate) fn object_format() -> ObjectFormatInfo {
    ObjectFormatInfo {
        elf_machine: EM_AARCH64,
        elf_class: ElfClass::Elf64,
        elf_flags: 0,
        absolute_reloc: |width| match width {
            4 => Some(R_AARCH64_ABS32),
            8 => Some(R_AARCH64_ABS64),
            _ => None,
        },
        reloc_for: |op| match op {
            "adr" => Some(RelocKind {
                r_type: R_AARCH64_ADR_PREL_LO21,
                addend: 0,
                field_offset: 0,
            }),
            "bl" => Some(RelocKind {
                r_type: R_AARCH64_CALL26,
                addend: 0,
                field_offset: 0,
            }),
            "b" => Some(RelocKind {
                r_type: R_AARCH64_JUMP26,
                addend: 0,
                field_offset: 0,
            }),
            "b.eq" | "b.ne" | "b.lt" | "b.ge" | "b.lo" | "b.hs" | "b.gt" | "b.le" | "b.hi"
            | "b.ls" | "b.mi" | "b.pl" | "b.vs" | "b.vc" | "cbz" | "cbnz" => Some(RelocKind {
                r_type: R_AARCH64_CONDBR19,
                addend: 0,
                field_offset: 0,
            }),
            "tbz" | "tbnz" => Some(RelocKind {
                r_type: R_AARCH64_TSTBR14,
                addend: 0,
                field_offset: 0,
            }),
            _ => None,
        },
        // AArch64 branch immediates are word offsets; adr uses byte offsets.
        pc_rel_scale: |op| if op == "adr" { 0 } else { 2 },
        pc_rel_from_end: |_| false,
    }
}
