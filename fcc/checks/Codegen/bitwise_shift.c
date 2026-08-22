// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_bitwise_shift.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @bits
// CHECK: andi
// CHECK: xori
// CHECK: ori
// CHECK: shli
// CHECK: shrui
// CHECK: %{{[0-9]+}} = func.func @signed_shift
// CHECK: shrsi
