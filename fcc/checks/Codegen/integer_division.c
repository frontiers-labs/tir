// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_integer_division.c | filecheck %s

// CHECK: divsi
// CHECK: divui
