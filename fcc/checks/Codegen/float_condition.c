// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_condition.c | filecheck %s

// CHECK: %[[BITS:[0-9]+]] = constant {value = 0} : !i32
// CHECK-NEXT: bitcast %[[BITS]] : !f32
// CHECK: fp.cmp {{%[0-9]+}}, {{%[0-9]+}} {predicate = "une"} : !i1
