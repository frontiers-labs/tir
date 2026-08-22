// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_character.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @ordinary
// CHECK: constant {value = 65}
// CHECK: %{{[0-9]+}} = func.func @escaped
// CHECK: constant {value = 10}
