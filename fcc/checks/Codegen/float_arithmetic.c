// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_float_arithmetic.c | tir opt --verify | filecheck %s

// CHECK: func.func @add({{%[0-9]+}}: !f32, {{%[0-9]+}}: !f32) -> !f32
// CHECK: ptr.alloca {size = 4, align = 4}
// CHECK: ptr.store {{%[0-9]+}}, {{%[0-9]+}}
// CHECK: ptr.load {{%[0-9]+}} : !f32
// CHECK: %[[LEFT:[0-9]+]] = fp.capture %{{[0-9]+}} : !fp.computation<!f32, nearest_even, separate>
// CHECK: %[[RIGHT:[0-9]+]] = fp.capture %{{[0-9]+}} : !fp.computation<!f32, nearest_even, separate>
// CHECK: %[[SUM:[0-9]+]] = fp.add %[[LEFT]], %[[RIGHT]] : !fp.computation<!f32, nearest_even, separate>
// CHECK: fp.materialize %[[SUM]] : !f32
