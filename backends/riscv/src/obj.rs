//! RISC-V ELF format parameters and relocation selection.

use tir::backend::binary::{EM_RISCV, ElfClass, ObjectFormatInfo, RelocKind};

const R_RISCV_BRANCH: u32 = 16;
const R_RISCV_32: u32 = 1;
const R_RISCV_64: u32 = 2;
const R_RISCV_JAL: u32 = 17;
const R_RISCV_HI20: u32 = 26;
const R_RISCV_LO12_I: u32 = 27;
const R_RISCV_RVC_BRANCH: u32 = 44;
const R_RISCV_RVC_JUMP: u32 = 45;

const EF_RISCV_RVC: u32 = 0x1;
const EF_RISCV_FLOAT_ABI_SINGLE: u32 = 0x2;
const EF_RISCV_FLOAT_ABI_DOUBLE: u32 = 0x4;

pub(crate) fn object_format(xlen: u32, features: &[crate::Feature]) -> ObjectFormatInfo {
    // e_flags declare the ABI the object was built for; linkers refuse to mix
    // float ABIs, so F/D-extension targets must claim their hardware float ABI.
    let mut elf_flags = 0;
    if features.contains(&crate::Feature::C) {
        elf_flags |= EF_RISCV_RVC;
    }
    if features.contains(&crate::Feature::D) {
        elf_flags |= EF_RISCV_FLOAT_ABI_DOUBLE;
    } else if features.contains(&crate::Feature::F) {
        elf_flags |= EF_RISCV_FLOAT_ABI_SINGLE;
    }
    ObjectFormatInfo {
        elf_machine: EM_RISCV,
        elf_class: if xlen == 64 {
            ElfClass::Elf64
        } else {
            ElfClass::Elf32
        },
        elf_flags,
        absolute_reloc: |width| match width {
            4 => Some(R_RISCV_32),
            8 => Some(R_RISCV_64),
            _ => None,
        },
        reloc_for: |op| match op {
            "jal" => Some(RelocKind {
                r_type: R_RISCV_JAL,
                addend: 0,
                field_offset: 0,
            }),
            "beq" | "bne" | "blt" | "bge" | "bltu" | "bgeu" => Some(RelocKind {
                r_type: R_RISCV_BRANCH,
                addend: 0,
                field_offset: 0,
            }),
            // Compressed control flow only reaches the encoder through
            // hand-written assembly; codegen never compresses fixup-carrying
            // instructions (no branch relaxation exists for their short
            // ranges).
            "c.j" | "c.jal" => Some(RelocKind {
                r_type: R_RISCV_RVC_JUMP,
                addend: 0,
                field_offset: 0,
            }),
            "c.beqz" | "c.bnez" => Some(RelocKind {
                r_type: R_RISCV_RVC_BRANCH,
                addend: 0,
                field_offset: 0,
            }),
            // Symbol-operand `lui`/`addi` only come from `lower_addr_of`'s
            // absolute-address pair; immediate forms never carry fixups.
            "lui" => Some(RelocKind {
                r_type: R_RISCV_HI20,
                addend: 0,
                field_offset: 0,
            }),
            "addi" => Some(RelocKind {
                r_type: R_RISCV_LO12_I,
                addend: 0,
                field_offset: 0,
            }),
            _ => None,
        },
        // RISC-V branch immediates are byte offsets (bit 0 implicit in the
        // encoding's scattering), so deltas are patched unscaled.
        pc_rel_scale: |_| 0,
        pc_rel_from_end: |_| false,
    }
}
