// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_floating_negation.c | filecheck %s

// CHECK: constant {value = -9223372036854775808} : !i64
// CHECK: bitcast {{%[0-9]+}} : !i64
// CHECK: xori {{%[0-9]+}}, {{%[0-9]+}} : !i64
// CHECK: bitcast {{%[0-9]+}} : !f64
// CHECK: constant {value = -2147483648} : !i32
// CHECK: bitcast {{%[0-9]+}} : !i32
// CHECK: xori {{%[0-9]+}}, {{%[0-9]+}} : !i32
// CHECK: bitcast {{%[0-9]+}} : !f32
