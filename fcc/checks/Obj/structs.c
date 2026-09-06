// RUN: fcc compile -O2 --stage obj --march riscv64 -o - %S/../Inputs/structs.c | tir readobj - | filecheck %s --check-prefix=RV
// RUN: fcc compile -O2 --stage obj --march arm64 -o - %S/../Inputs/structs.c | tir readobj - | filecheck %s --check-prefix=A64
// RUN: fcc compile -O2 --stage asm --march riscv64 -o - %S/../Inputs/structs.c | filecheck %s --check-prefix=RVASM
// RUN: fcc compile -O2 --stage asm --march arm64 -o - %S/../Inputs/structs.c | filecheck %s --check-prefix=A64ASM

// RV: Machine: EM_RISCV (243)
// RV: Symbol read:
// RV: Symbol copy:

// A64: Machine: EM_AARCH64 (183)
// A64: Symbol read:
// A64: Symbol copy:

// `copy` writes one field of a local and copies the whole struct into another
// local; neither leaves the function. The mid-end forwards the 37 through the
// field-by-field copy to the return and drops the destination, so only the
// store of 37 into the source slot remains, with no byte copy.
// RVASM: read:
// RVASM-NEXT: {{(c\.)?lw}} {{.*}}, 4({{.*}})
// RVASM: copy:
// RVASM: addi x10, x0, 37
// RVASM-NOT: lb
// RVASM-NOT: sb
// RVASM: sw x10, 4({{.*}})
// RVASM-NOT: lw
// RVASM: c.jr x1

// A64ASM: read:
// A64ASM-NEXT: ldr {{.*}}, [{{.*}}, 4]
// A64ASM: copy:
// A64ASM: movz x0, 37
// A64ASM-NOT: ldrb
// A64ASM-NOT: strb
// A64ASM: str x0, [{{.*}}, 4]
// A64ASM-NOT: ldr
// A64ASM: ret x30
