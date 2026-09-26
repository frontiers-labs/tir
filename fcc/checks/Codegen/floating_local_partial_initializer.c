// RUN: fcc compile --stage ir -o - %s | filecheck %s

float tail(void) {
    float values[3] = {1.0f};
    return values[2];
}

// CHECK-LABEL: func.func @tail
// CHECK: constant {value = 0} : !i32
// CHECK: bitcast {{%[0-9]+}} : !f32
