// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_unary.c | filecheck %s

// CHECK: func.func @negate
// CHECK: subi
// CHECK: func.func @complement
// CHECK: xori
// CHECK: func.func @logical_not
// CHECK: cmpi
// CHECK: extui
// CHECK: func.func @positive
