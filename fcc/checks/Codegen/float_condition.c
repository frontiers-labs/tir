// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_condition.c | filecheck %s

// CHECK: constant {value = 0} : !i32
// CHECK: bitcast {{%[0-9]+}} : !f32
// CHECK: fp.cmp {{%[0-9]+}}, {{%[0-9]+}} {predicate = "une"} : !i1
