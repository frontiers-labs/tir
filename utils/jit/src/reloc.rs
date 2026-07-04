//! In-memory application of ELF relocations, keyed by `(e_machine, r_type)`.
//!
//! The object writer emits standard ELF relocation types; a static linker would
//! resolve them. A JIT resolves them itself against runtime addresses, so this
//! module reimplements the arithmetic for the handful of relocations codegen
//! produces on x86-64 and AArch64. Constants are the architectural ELF numbers.

use crate::JitError;
use tir::backend::binary::{EM_AARCH64, EM_RISCV, EM_X86_64};

const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;

const R_AARCH64_ADR_PREL_LO21: u32 = 274;
const R_AARCH64_CONDBR19: u32 = 280;
const R_AARCH64_JUMP26: u32 = 282;
const R_AARCH64_CALL26: u32 = 283;

const R_RISCV_BRANCH: u32 = 16;
const R_RISCV_JAL: u32 = 17;

/// Whether a relocation targets a branch/call immediate whose limited range
/// means an external (host) symbol must be reached through a trampoline rather
/// than a direct displacement.
pub fn needs_trampoline(machine: u16, r_type: u32) -> bool {
    match machine {
        EM_X86_64 => r_type == R_X86_64_PLT32,
        EM_AARCH64 => matches!(
            r_type,
            R_AARCH64_CALL26 | R_AARCH64_JUMP26 | R_AARCH64_CONDBR19
        ),
        EM_RISCV => matches!(r_type, R_RISCV_JAL | R_RISCV_BRANCH),
        _ => false,
    }
}

/// Patch the relocated field at `site` (its runtime address is `p`) so it
/// references symbol address `s`, per the ELF relocation `r_type`.
pub fn apply(
    machine: u16,
    r_type: u32,
    site: *mut u8,
    s: u64,
    p: u64,
    addend: i64,
) -> Result<(), JitError> {
    let s = s as i64;
    let p = p as i64;
    match (machine, r_type) {
        (EM_X86_64, R_X86_64_PC32 | R_X86_64_PLT32) => {
            let value = s + addend - p;
            if !fits_signed(value, 32) {
                return Err(JitError::RelocRange { r_type, value });
            }
            write_u32(site, value as u32);
        }
        (EM_AARCH64, R_AARCH64_CALL26 | R_AARCH64_JUMP26) => {
            let value = s + addend - p;
            let scaled = value >> 2;
            if value & 0b11 != 0 || !fits_signed(scaled, 26) {
                return Err(JitError::RelocRange { r_type, value });
            }
            patch_u32(site, 0x03FF_FFFF, (scaled as u32) & 0x03FF_FFFF);
        }
        (EM_AARCH64, R_AARCH64_CONDBR19) => {
            let value = s + addend - p;
            let scaled = value >> 2;
            if value & 0b11 != 0 || !fits_signed(scaled, 19) {
                return Err(JitError::RelocRange { r_type, value });
            }
            patch_u32(site, 0x7FFFF << 5, ((scaled as u32) & 0x7FFFF) << 5);
        }
        (EM_AARCH64, R_AARCH64_ADR_PREL_LO21) => {
            let value = s + addend - p;
            if !fits_signed(value, 21) {
                return Err(JitError::RelocRange { r_type, value });
            }
            let immlo = (value as u32) & 0b11;
            let immhi = ((value >> 2) as u32) & 0x7FFFF;
            patch_u32(
                site,
                (0b11 << 29) | (0x7FFFF << 5),
                (immlo << 29) | (immhi << 5),
            );
        }
        (EM_RISCV, R_RISCV_JAL) => {
            let value = s + addend - p;
            if value & 0b1 != 0 || !fits_signed(value, 21) {
                return Err(JitError::RelocRange { r_type, value });
            }
            let v = value as u32;
            let imm = ((v >> 20) & 0x1) << 31
                | ((v >> 1) & 0x3FF) << 21
                | ((v >> 11) & 0x1) << 20
                | ((v >> 12) & 0xFF) << 12;
            patch_u32(site, 0xFFFF_F000, imm);
        }
        (EM_RISCV, R_RISCV_BRANCH) => {
            let value = s + addend - p;
            if value & 0b1 != 0 || !fits_signed(value, 13) {
                return Err(JitError::RelocRange { r_type, value });
            }
            let v = value as u32;
            let imm = ((v >> 12) & 0x1) << 31
                | ((v >> 5) & 0x3F) << 25
                | ((v >> 1) & 0xF) << 8
                | ((v >> 11) & 0x1) << 7;
            patch_u32(site, 0xFE00_0F80, imm);
        }
        _ => return Err(JitError::RelocUnsupported { machine, r_type }),
    }
    Ok(())
}

/// The trampoline for `EM_*`: load the 64-bit host address and jump to it.
/// Returned bytes are self-contained; `addr` is the absolute target.
pub fn trampoline(machine: u16, addr: u64) -> Result<Vec<u8>, JitError> {
    let mut code = Vec::new();
    match machine {
        EM_X86_64 => {
            // movabs r11, addr ; jmp r11
            code.extend_from_slice(&[0x49, 0xBB]);
            code.extend_from_slice(&addr.to_le_bytes());
            code.extend_from_slice(&[0x41, 0xFF, 0xE3]);
        }
        EM_AARCH64 => {
            // ldr x16, #8 ; br x16 ; .quad addr
            code.extend_from_slice(&0x5800_0050u32.to_le_bytes());
            code.extend_from_slice(&0xD61F_0200u32.to_le_bytes());
            code.extend_from_slice(&addr.to_le_bytes());
        }
        _ => return Err(JitError::RelocUnsupported { machine, r_type: 0 }),
    }
    Ok(code)
}

/// Byte size of a trampoline slot for the target, or `None` if the target has
/// no trampoline support.
pub fn trampoline_size(machine: u16) -> Option<usize> {
    match machine {
        EM_X86_64 => Some(13),
        EM_AARCH64 => Some(16),
        _ => None,
    }
}

/// Make freshly written code at `[ptr, ptr+len)` visible to instruction fetch.
/// A no-op on x86-64 (coherent I-cache); AArch64 needs explicit maintenance.
pub fn flush_icache(ptr: *const u8, len: usize) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::asm;
        // Cache line sizes come from CTR_EL0: bits [19:16] are log2 words for
        // D-cache, [3:0] for I-cache. Clean each D line, invalidate each I line.
        let mut ctr: u64;
        asm!("mrs {}, ctr_el0", out(reg) ctr);
        let dline = 4usize << ((ctr >> 16) & 0xF);
        let iline = 4usize << (ctr & 0xF);
        let end = ptr as usize + len;

        let mut addr = (ptr as usize) & !(dline - 1);
        while addr < end {
            asm!("dc cvau, {}", in(reg) addr);
            addr += dline;
        }
        asm!("dsb ish");

        let mut addr = (ptr as usize) & !(iline - 1);
        while addr < end {
            asm!("ic ivau, {}", in(reg) addr);
            addr += iline;
        }
        asm!("dsb ish", "isb");
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (ptr, len);
    }
}

fn fits_signed(value: i64, bits: u32) -> bool {
    let min = -(1i64 << (bits - 1));
    let max = (1i64 << (bits - 1)) - 1;
    value >= min && value <= max
}

fn write_u32(site: *mut u8, value: u32) {
    unsafe {
        std::ptr::copy_nonoverlapping(value.to_le_bytes().as_ptr(), site, 4);
    }
}

fn patch_u32(site: *mut u8, clear_mask: u32, set_bits: u32) {
    let mut word = [0u8; 4];
    unsafe {
        std::ptr::copy_nonoverlapping(site, word.as_mut_ptr(), 4);
    }
    let value = (u32::from_le_bytes(word) & !clear_mask) | set_bits;
    write_u32(site, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_at(buf: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*buf)
    }

    #[test]
    fn x86_pc32_backward() {
        // call at P=0x1010 targeting S=0x1000, addend -4: disp = -0x14.
        let mut buf = [0u8; 4];
        apply(
            EM_X86_64,
            R_X86_64_PLT32,
            buf.as_mut_ptr(),
            0x1000,
            0x1010,
            -4,
        )
        .unwrap();
        assert_eq!(i32::from_le_bytes(buf), -0x14);
    }

    #[test]
    fn aarch64_call26_forward() {
        // bl at P=0x1000 to S=0x1010: (0x10)>>2 = 4 in imm26; opcode 0x94000000.
        let mut buf = 0x9400_0000u32.to_le_bytes();
        apply(
            EM_AARCH64,
            R_AARCH64_CALL26,
            buf.as_mut_ptr(),
            0x1010,
            0x1000,
            0,
        )
        .unwrap();
        assert_eq!(word_at(&buf), 0x9400_0004);
    }

    #[test]
    fn aarch64_call26_backward() {
        // bl at P=0x1010 to S=0x1000: delta -0x10, >>2 = -4, imm26 two's complement.
        let mut buf = 0x9400_0000u32.to_le_bytes();
        apply(
            EM_AARCH64,
            R_AARCH64_CALL26,
            buf.as_mut_ptr(),
            0x1000,
            0x1010,
            0,
        )
        .unwrap();
        assert_eq!(word_at(&buf), 0x9400_0000 | (0x03FF_FFFF & (-4i32 as u32)));
    }

    #[test]
    fn aarch64_condbr19() {
        // b.cond at P=0x2000 to S=0x2008: (8)>>2 = 2 in imm19 at bits [23:5].
        let mut buf = 0x5400_0000u32.to_le_bytes();
        apply(
            EM_AARCH64,
            R_AARCH64_CONDBR19,
            buf.as_mut_ptr(),
            0x2008,
            0x2000,
            0,
        )
        .unwrap();
        assert_eq!(word_at(&buf), 0x5400_0000 | (2 << 5));
    }

    #[test]
    fn aarch64_range_error() {
        // Beyond ±128 MiB is out of CALL26 range.
        let mut buf = 0x9400_0000u32.to_le_bytes();
        let err = apply(
            EM_AARCH64,
            R_AARCH64_CALL26,
            buf.as_mut_ptr(),
            0x1000_0000_0000,
            0,
            0,
        );
        assert!(matches!(err, Err(JitError::RelocRange { .. })));
    }

    #[test]
    fn trampolines_are_well_formed() {
        let x86 = trampoline(EM_X86_64, 0xDEAD_BEEF_CAFE_F00D).unwrap();
        assert_eq!(&x86[..2], &[0x49, 0xBB]);
        assert_eq!(&x86[2..10], &0xDEAD_BEEF_CAFE_F00Du64.to_le_bytes());
        assert_eq!(&x86[10..], &[0x41, 0xFF, 0xE3]);
        assert_eq!(x86.len(), trampoline_size(EM_X86_64).unwrap());

        let arm = trampoline(EM_AARCH64, 0x1234_5678_9ABC_DEF0).unwrap();
        assert_eq!(word_at(&arm[0..4].try_into().unwrap()), 0x5800_0050);
        assert_eq!(word_at(&arm[4..8].try_into().unwrap()), 0xD61F_0200);
        assert_eq!(&arm[8..], &0x1234_5678_9ABC_DEF0u64.to_le_bytes());
        assert_eq!(arm.len(), trampoline_size(EM_AARCH64).unwrap());
    }
}
