// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_if.c | filecheck %s

// CHECK: scf.if %{{[0-9]+}} {
// CHECK: ptr.store
// CHECK: scf.yield
// CHECK: else {
// CHECK: ptr.store
// CHECK: scf.yield
