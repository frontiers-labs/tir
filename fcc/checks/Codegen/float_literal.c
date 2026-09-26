// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_literal.c | filecheck %s

// CHECK: constant {value = 4609434218613702656} : !i64
// CHECK: bitcast {{.*}} : !f64
// CHECK: constant {value = 4598175219545276416} : !i64
// CHECK: bitcast {{.*}} : !f64
// CHECK: constant {value = 4611686018427387904} : !i64
// CHECK: bitcast {{.*}} : !f64
// CHECK: constant {value = 4636737291354636288} : !i64
// CHECK: bitcast {{.*}} : !f64
