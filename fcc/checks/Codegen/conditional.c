// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_conditional.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @conditional
// CHECK: scf.if
// CHECK: addi
// CHECK: scf.yield
// CHECK: else
// CHECK: addi
// CHECK: scf.yield
