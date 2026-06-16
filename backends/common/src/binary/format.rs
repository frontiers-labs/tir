//! Target-provided parameters for object-file emission.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfClass {
    Elf32,
    Elf64,
}

/// How an unresolved symbol fixup is expressed in the object file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelocKind {
    /// ELF relocation type (`r_type`).
    pub r_type: u32,
    pub addend: i64,
}

/// Everything the generic object writer needs to know about a target's
/// object format. Returned by `TargetMachine` implementations.
#[derive(Clone, Copy)]
pub struct ObjectFormatInfo {
    /// ELF `e_machine` (e.g. `EM_RISCV`, `EM_AARCH64`).
    pub elf_machine: u16,
    pub elf_class: ElfClass,
    /// ELF `e_flags`.
    pub elf_flags: u32,
    /// Maps an op name to the relocation used for its symbol operand;
    /// `None` means the instruction cannot take a symbol operand.
    pub reloc_for: fn(&str) -> Option<RelocKind>,
    /// log2 of the divisor applied to a pc-relative byte delta before it is
    /// scattered into the instruction (0 on RISC-V; 2 on AArch64, whose
    /// branch immediates are word offsets).
    pub pc_rel_scale: fn(&str) -> u8,
    /// Byte offset, from the start of an instruction, of the field a symbol
    /// relocation applies to. Zero on fixed-width RISC targets, where the
    /// relocation covers the whole instruction word; on x86 it skips the
    /// opcode bytes so the relocation lands on the trailing displacement.
    pub reloc_field_offset: fn(&str) -> u8,
}
