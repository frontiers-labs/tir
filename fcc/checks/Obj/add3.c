// RUN: fcc compile --stage obj --march riscv64 -o - %S/../Inputs/add3.c | tir readobj - | filecheck %s
// RUN: fcc compile --stage obj --march arm64 -o - %S/../Inputs/add3.c | tir readobj - | filecheck %s --check-prefix=A64
// RUN: fcc compile --stage asm --march riscv64 -o - %S/../Inputs/add3.c | filecheck %s --check-prefix=ASM

// The bare riscv64 march enables the generic everything-on profile, so the
// return sequence compresses to the 2-byte c.jr.
// CHECK: File: ELF64 LSB REL
// CHECK: Machine: EM_RISCV (243)
// CHECK: Section .text: type=PROGBITS flags=AX size=0xa align=4
// CHECK: Symbol add3: value=0x0 size=0xa bind=GLOBAL type=FUNC section=.text

// A64: File: ELF64 LSB REL
// A64: Machine: EM_AARCH64 (183)
// A64: Symbol add3: value=0x0 size=0xc bind=GLOBAL type=FUNC section=.text

// ASM: .global add3
// ASM: add3:
// ASM: addw
// ASM: addiw
// ASM: c.jr x1
