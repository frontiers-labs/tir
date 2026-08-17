// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_conditional.c | filecheck %s

// CHECK: func.func @conditional
// CHECK: cir.if
// CHECK: addi
// CHECK: ptr.store
// CHECK: else
// CHECK: addi
// CHECK: ptr.store
