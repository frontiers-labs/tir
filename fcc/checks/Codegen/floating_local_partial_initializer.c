// Aggregate codegen requires layout facts; executable layout regions were removed.
// XFAIL: *
// RUN: fcc compile --stage ir -o - %s | filecheck %s

float tail(void) {
    float values[3] = {1.0f};
    return values[2];
}

// CHECK-LABEL: func.func @tail
// CHECK: fp.constant {bits = 0} : !f32
