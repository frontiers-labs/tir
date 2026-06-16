// RUN: fcc compile --stage obj --march riscv64 -o - %S/../Inputs/branch_select.c | tir readobj - | filecheck %s
// RUN: fcc compile --stage obj --march arm64 -o - %S/../Inputs/branch_select.c | tir readobj - | filecheck %s --check-prefix=A64

// An `if`/`else` lowers to a CFG whose conditional branch is fused from the
// `cmpi`, and the whole function encodes to a valid relocatable object on both
// targets.

// CHECK: File: ELF64 LSB REL
// CHECK: Machine: EM_RISCV (243)
// CHECK: Section .text: type=PROGBITS flags=AX size=0x18 align=4
// CHECK: Symbol pick: value=0x0 size=0x18 bind=GLOBAL type=FUNC section=.text

// A64: File: ELF64 LSB REL
// A64: Machine: EM_AARCH64 (183)
// A64: Section .text: type=PROGBITS flags=AX size=0x1c align=4
// A64: Symbol pick: value=0x0 size=0x1c bind=GLOBAL type=FUNC section=.text
