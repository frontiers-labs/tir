// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_while.c | filecheck %s

// CHECK: scf.while {
// CHECK: cmpi {{.*}} {predicate = "slt"}
// CHECK: scf.if
// CHECK: scf.condition
// CHECK: do {
// CHECK: scf.yield
