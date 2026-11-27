// RUN: fcc compile --stage ir -o - %s | filecheck %s

float increment(float value) {
    return ++value;
}

// CHECK-LABEL: func.func @increment
// CHECK: %[[BITS:[0-9]+]] = constant {value = 1065353216} : !i32
// CHECK-NEXT: bitcast %[[BITS]] : !f32
// CHECK: fp.add {{%[0-9]+}}, {{%[0-9]+}} : !fp.computation<!f32, nearest_even, separate>
// CHECK: fp.materialize
