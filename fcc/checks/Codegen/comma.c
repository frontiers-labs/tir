// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_comma.c | filecheck %s

// CHECK: func.func @comma_value
// CHECK: constant {value = 3}
// CHECK: ptr.store
// CHECK: ptr.load
// CHECK: addi
