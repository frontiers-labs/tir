// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_integer_remainder.c | filecheck %s

// CHECK: remsi
// CHECK: remui
