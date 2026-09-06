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
// local; neither leaves the function. The copy is spelled field by field
// through `ptradd` offsets into the two slots, and promote-nodes promotes whole
// slots, not fields, so the 37 travels through memory: stored at offset 4 of
// the source, reloaded there, stored at offset 4 of the destination, and the
// reload is what is returned. Per-object chains let the block-based mid-end
// fold the whole function to the literal (`addi 37` / `movz 37`, no store).
// RVASM: read:
// RVASM-NEXT: {{(c\.)?lw}} {{.*}}, 4({{.*}})
// RVASM: copy:
// RVASM: addi [[V:x[0-9]+]], x0, 37
// RVASM: sw [[V]], 4({{.*}})
// RVASM: lb
// RVASM: sb
// RVASM: lw x10, 4({{.*}})
// RVASM: sw x10, 4({{.*}})
// RVASM: c.jr x1

// A64ASM: read:
// A64ASM-NEXT: ldr {{.*}}, [{{.*}}, 4]
// A64ASM: copy:
// A64ASM: movz [[V:x[0-9]+]], 37
// A64ASM: str [[V]], [{{.*}}, 4]
// A64ASM: ldrb
// A64ASM: strb
// A64ASM: ldr x0, [{{.*}}, 4]
// A64ASM: str x0, [{{.*}}, 4]
// A64ASM: ret x30
