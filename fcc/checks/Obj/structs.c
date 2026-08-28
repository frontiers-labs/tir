// RUN: fcc compile --stage obj --march riscv64 -o - %S/../Inputs/structs.c | tir readobj - | filecheck %s --check-prefix=RV
// RUN: fcc compile --stage obj --march arm64 -o - %S/../Inputs/structs.c | tir readobj - | filecheck %s --check-prefix=A64
// RUN: fcc compile --stage asm --march riscv64 -o - %S/../Inputs/structs.c | filecheck %s --check-prefix=RVASM
// RUN: fcc compile --stage asm --march arm64 -o - %S/../Inputs/structs.c | filecheck %s --check-prefix=A64ASM

// RV: Machine: EM_RISCV (243)
// RV: Symbol read:
// RV: Symbol copy:

// A64: Machine: EM_AARCH64 (183)
// A64: Symbol read:
// A64: Symbol copy:

// `copy` writes one field of a local and copies the whole struct into another
// local; neither leaves the function, so nothing observes the memory and the
// return value is the literal it was given.
// RVASM: read:
// RVASM-NEXT: {{(c\.)?lw}} {{.*}}, 4({{.*}})
// RVASM: copy:
// RVASM-NEXT: addi {{.*}}, {{.*}}, 37
// RVASM-NOT: sw

// A64ASM: read:
// A64ASM-NEXT: ldr {{.*}}, [{{.*}}, 4]
// A64ASM: copy:
// A64ASM-NEXT: movz {{.*}}, 37
// A64ASM-NOT: str
