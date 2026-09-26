// RUN: fcc compile --stage ir -o - %s | filecheck %s

float increment(float value) {
    return ++value;
}

// CHECK-LABEL: func.func @increment
// CHECK: constant {value = 1065353216} : !i32
// CHECK: bitcast {{%[0-9]+}} : !f32
// CHECK: fp.add {{%[0-9]+}}, {{%[0-9]+}} : !f32
