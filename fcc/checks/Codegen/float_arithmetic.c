// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_arithmetic.c | tir opt --verify | filecheck %s

// CHECK: func.func @add(%[[A:[0-9]+]]: !f32, %[[B:[0-9]+]]: !f32) -> !f32
// CHECK: %[[LEFT_SLOT:[0-9]+]] = ptr.alloca {size = 4, align = 4}
// CHECK: %[[RIGHT_SLOT:[0-9]+]] = ptr.alloca {size = 4, align = 4}
// CHECK-DAG: %[[LEFT_STATE:[0-9]+]] = ptr.store %[[A]], %[[LEFT_SLOT]] state(%{{[0-9]+}})
// CHECK-DAG: %[[RIGHT_STATE:[0-9]+]] = ptr.store %[[B]], %[[RIGHT_SLOT]] state(%{{[0-9]+}})
// CHECK: %[[LEFT_VALUE:[0-9]+]], %{{[0-9]+}} = ptr.load %[[LEFT_SLOT]] state(%[[LEFT_STATE]]) : !f32
// CHECK: %[[RIGHT_VALUE:[0-9]+]], %{{[0-9]+}} = ptr.load %[[RIGHT_SLOT]] state(%[[RIGHT_STATE]]) : !f32
// CHECK: %[[LEFT:[0-9]+]] = fp.capture %[[LEFT_VALUE]] : !fp.computation<!f32, nearest_even, separate>
// CHECK: %[[RIGHT:[0-9]+]] = fp.capture %[[RIGHT_VALUE]] : !fp.computation<!f32, nearest_even, separate>
// CHECK: %[[SUM:[0-9]+]] = fp.add %[[LEFT]], %[[RIGHT]] : !fp.computation<!f32, nearest_even, separate>
// CHECK: fp.materialize %[[SUM]] : !f32
