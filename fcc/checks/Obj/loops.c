// RUN: fcc compile --stage obj --march riscv32 -o - %S/../Inputs/basic_for.c | tir readobj - | filecheck %s
// RUN: fcc compile --stage asm --march riscv32 -o - %S/../Inputs/long_counted_loop.c | filecheck %s --check-prefix=ASM
// RUN: fcc compile --stage asm --march riscv32 -o - %S/../Inputs/loop_control.c | filecheck %s --check-prefix=CONTROL

// CHECK: File: ELF32 LSB REL
// CHECK: Symbol count: value=0x0

// A counted loop destructs rotated: the zero-trip guard and the latch are both
// the same signed comparison against the bound, fused into their branches. The
// loop has to be longer than the scheduler unrolls whole, or there is no loop
// left in the assembly to look at — `basic_for.c`'s three iterations fold to the
// value they end on.
// ASM: count_long:
// ASM: blt
// ASM: jal

// CONTROL: stop_early:
// CONTROL: bne
// CONTROL: jal
