// RUN: fcc compile --stage obj --march riscv64 -o - %S/../Inputs/hello_stdio.c | tir readobj - | filecheck %s
// RUN: fcc compile --stage asm --march riscv64 -o - %S/../Inputs/hello_stdio.c | filecheck %s --check-prefix=ASM
// RUN: fcc compile --stage obj --march arm64 -o - %S/../Inputs/hello_stdio.c | tir readobj - | filecheck %s --check-prefix=ARM
// RUN: fcc compile --stage asm --march arm64 -o - %S/../Inputs/hello_stdio.c | filecheck %s --check-prefix=ARMASM
// RUN: fcc compile --stage obj --march x86_64 -o - %S/../Inputs/hello_stdio.c | tir readobj - | filecheck %s --check-prefix=X86
// RUN: fcc compile --stage asm --march x86_64 -o - %S/../Inputs/hello_stdio.c | filecheck %s --check-prefix=X86ASM

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

// ARM: File: ELF64 LSB REL
// ARM: Machine: EM_AARCH64 (183)
// ARM: Section .text: type=PROGBITS flags=AX
// ARM: Section .rodata: type=PROGBITS flags=A size=0xe align=1
// ARM: Symbol .L.str0: value=0x0 size=0xe bind=LOCAL type=OBJECT section=.rodata
// ARM: Symbol main: value=0x0 size={{0x[0-9a-f]+}} bind=GLOBAL type=FUNC section=.text
// ARM: Symbol printf: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
// ARM: Reloc .text+0x4: R_AARCH64_ADR_PREL_LO21 .L.str0 + 0
// ARM: Reloc .text+{{0x[0-9a-f]+}}: R_AARCH64_CALL26 printf + 0

// ARMASM: .global main
// ARMASM: main:
// ARMASM: adr {{x[0-9]+}}, .L.str0
// ARMASM: bl printf
// ARMASM: .section .rodata
// ARMASM: .L.str0:
// ARMASM: .asciz "hello, world\n"

// The string address materializes as a rip-relative lea relocated against the
// local symbol; the printf call relocates as PLT32. The call is bracketed by an
// 8-byte stack adjustment that realigns rsp to 16 bytes at the call site.
// X86: File: ELF64 LSB REL
// X86: Machine: EM_X86_64 (62)
// X86: Section .text: type=PROGBITS flags=AX
// X86: Section .rodata: type=PROGBITS flags=A size=0xe align=1
// X86: Symbol .L.str0: value=0x0 size=0xe bind=LOCAL type=OBJECT section=.rodata
// X86: Symbol main: value=0x0 size={{0x[0-9a-f]+}} bind=GLOBAL type=FUNC section=.text
// X86: Symbol printf: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
// X86: Reloc .text+0x3: R_X86_64_PC32 .L.str0 + -4
// X86: Reloc .text+{{0x[0-9a-f]+}}: R_X86_64_PLT32 printf + -4

// X86ASM: .global main
// X86ASM: main:
// X86ASM: lea {{r[a-z0-9]+}}, [rip + .L.str0]
// X86ASM: call printf
// X86ASM: .section .rodata
// X86ASM: .L.str0:
// X86ASM: .asciz "hello, world\n"
