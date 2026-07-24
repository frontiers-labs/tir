// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_literal.c | filecheck %s

// CHECK: constantf {value = 1.5}
// CHECK: constantf {value = 0.25}
// CHECK: constantf {value = 2.0}
// CHECK: constantf {value = 100.0}
