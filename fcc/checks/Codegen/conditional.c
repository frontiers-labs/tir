// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_conditional.c | filecheck %s

// CHECK: func.func @conditional
// CHECK: scf.if
// CHECK: addi
// CHECK: scf.yield
// CHECK: else
// CHECK: addi
// CHECK: scf.yield
