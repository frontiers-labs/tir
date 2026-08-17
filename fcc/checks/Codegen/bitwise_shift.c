// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_bitwise_shift.c | filecheck %s

// CHECK: func.func @bits
// CHECK: andi
// CHECK: xori
// CHECK: ori
// CHECK: shli
// CHECK: shrui
// CHECK: func.func @signed_shift
// CHECK: shrsi
