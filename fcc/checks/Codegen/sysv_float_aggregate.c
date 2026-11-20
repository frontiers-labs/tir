// Aggregate codegen requires layout facts; executable layout regions were removed.
// XFAIL: *
// RUN: fcc compile --march x86_64 --mabi sysv --stage ir -o - %s | filecheck %s

struct pair {
    float left;
    float right;
};

float sum_pair(struct pair value) {
    return value.left + value.right;
}

// CHECK-LABEL: func.func @sum_pair({{%[0-9]+}}: !f64) -> !f32
