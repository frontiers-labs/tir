// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_logical.c | filecheck %s

// CHECK: func.func @logical_and
// CHECK: scf.if
// CHECK: addi
// CHECK: scf.yield
// CHECK: else
// CHECK: scf.yield
// CHECK: func.func @logical_or
// CHECK: scf.if
// CHECK: scf.yield
// CHECK: else
// CHECK: addi
// CHECK: scf.yield
