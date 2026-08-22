// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_increment.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @increment_values
// CHECK: addi
// CHECK: ptr.store
// CHECK: addi
// CHECK: ptr.store
// CHECK: subi
// CHECK: ptr.store
// CHECK: subi
// CHECK: ptr.store
