// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_do_while.c | filecheck %s

// A `do` is tail-controlled: the body runs before the condition, so both live
// in the condition region of the `scf.while`.

// CHECK: scf.while {
// CHECK: addi
// CHECK: ptr.store
// CHECK: cmpi {{.*}} {predicate = "slt"}
// CHECK: scf.condition
