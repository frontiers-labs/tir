// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_literal.c | filecheck %s

// CHECK: %[[A:[0-9]+]] = constant {value = 4609434218613702656} : !i64
// CHECK-NEXT: bitcast %[[A]] : !f64
// CHECK: %[[B:[0-9]+]] = constant {value = 4598175219545276416} : !i64
// CHECK-NEXT: bitcast %[[B]] : !f64
// CHECK: %[[C:[0-9]+]] = constant {value = 4611686018427387904} : !i64
// CHECK-NEXT: bitcast %[[C]] : !f64
// CHECK: %[[D:[0-9]+]] = constant {value = 4636737291354636288} : !i64
// CHECK-NEXT: bitcast %[[D]] : !f64
