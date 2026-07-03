// RUN: fcc compile --stage obj --march riscv64 -o - %S/../Inputs/hello_stdio.c | tir readobj - | filecheck %s
// RUN: fcc compile --stage asm --march riscv64 -o - %S/../Inputs/hello_stdio.c | filecheck %s --check-prefix=ASM

// The string literal lands in .rodata as a local object symbol; its address
// materializes as an absolute lui/addi pair relocated against the symbol, and
// the printf call relocates against the undefined libc symbol.
// CHECK: File: ELF64 LSB REL
// CHECK: Machine: EM_RISCV (243)
// CHECK: Section .text: type=PROGBITS flags=AX
// CHECK: Section .rodata: type=PROGBITS flags=A size=0xe align=1
// CHECK: Symbol .L.str0: value=0x0 size=0xe bind=LOCAL type=OBJECT section=.rodata
// CHECK: Symbol main: value=0x0 size={{0x[0-9a-f]+}} bind=GLOBAL type=FUNC section=.text
// CHECK: Symbol printf: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
// CHECK: Reloc .text+0x0: R_RISCV_HI20 .L.str0 + 0
// CHECK: Reloc .text+0x4: R_RISCV_LO12_I .L.str0 + 0
// CHECK: Reloc .text+{{0x[0-9a-f]+}}: R_RISCV_JAL printf + 0

// ASM: .global main
// ASM: main:
// ASM: lui {{x[0-9]+}}, .L.str0
// ASM: addi {{x[0-9]+}}, {{x[0-9]+}}, .L.str0
// ASM: jal x1, printf
// ASM: .section .rodata
// ASM: .L.str0:
// ASM: .asciz "hello, world\n"
