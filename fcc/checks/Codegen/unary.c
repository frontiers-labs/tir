// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_unary.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @negate
// CHECK: subi
// CHECK: %{{[0-9]+}} = func.func @complement
// CHECK: xori
// CHECK: %{{[0-9]+}} = func.func @logical_not
// CHECK: cmpi
// CHECK: extui
// CHECK: %{{[0-9]+}} = func.func @positive
