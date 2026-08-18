// RUN: fcc cc -S -march=riscv64 -o - %s | filecheck %s --check-prefix=RISCV
// RUN: fcc cc -E %s | filecheck %s --check-prefix=PP

// -march overrides the host default target, and preprocessing needs no target
// at all.

int add(int a, int b) { return a + b; }

// RISCV: add:
// RISCV: c.addw
// PP: int add(int a, int b) { return a + b{{;}} }
