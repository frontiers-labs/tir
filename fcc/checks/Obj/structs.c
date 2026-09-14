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

// RVASM: read:
// RVASM-NEXT: {{(c\.)?lw}} {{.*}}, 4({{.*}})
// RVASM: copy:
// RVASM: addi {{.*}}, x0, 37
// RVASM: sw {{.*}}, 4({{.*}})
// RVASM: jal x1, memcpy
// RVASM: lw x10, 4({{.*}})
// RVASM: c.jr x1

// A64ASM: read:
// A64ASM-NEXT: ldr {{.*}}, [{{.*}}, 4]
// A64ASM-NEXT: ret x30
// A64ASM: copy:
// A64ASM: movz {{.*}}, 37
// A64ASM: str {{.*}}, [{{.*}}, 4]
// A64ASM: bl memcpy
// A64ASM: ldr x0, [{{.*}}, 4]
// A64ASM: ret x30
