// RUN: fcc compile --stage ir -o - %s | filecheck %s

// An invariant loop still becomes a canonical scf.loop whose predicate is
// its original condition, never a branch.

int f(void) { while (1) {} return 0; }

// CHECK-NOT: cfg.
// CHECK: scf.loop {
// CHECK: %[[P:[0-9]+]] = cmpi
// CHECK-NOT: scf.switch
// CHECK: -> %[[P]]
// CHECK-NOT: cfg.
