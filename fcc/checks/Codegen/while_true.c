// RUN: fcc compile --stage ir -o - %s | filecheck %s

// An invariant loop still becomes a canonical scf.while, never a branch.

int f(void) { while (1) {} return 0; }

// CHECK-NOT: cfg.
// CHECK: scf.while {
// CHECK: scf.condition
// CHECK-NOT: cfg.
