// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_character.c | filecheck %s

// CHECK: func.func @ordinary
// CHECK: constant {value = 65}
// CHECK: func.func @escaped
// CHECK: constant {value = 10}
