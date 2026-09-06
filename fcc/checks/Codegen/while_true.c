// RUN: fcc compile --stage ir -o - %s | filecheck %s

// An invariant loop still becomes a canonical scf.loop whose predicate is
// the gamma over the condition, never a branch.

int f(void) { while (1) {} return 0; }

// CHECK-NOT: cfg.
// CHECK: scf.loop {
// CHECK: %[[P:[0-9]+]] = scf.switch
// CHECK: -> %[[P]]
// CHECK-NOT: cfg.
