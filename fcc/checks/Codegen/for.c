// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_for.c | filecheck %s

// CHECK: scf.while {
// CHECK: cmpi {{.*}} {predicate = "slt"}
// CHECK: scf.if
// CHECK: addi
// CHECK: ptr.store
// CHECK: scf.condition
