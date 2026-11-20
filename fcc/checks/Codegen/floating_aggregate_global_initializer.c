// Aggregate codegen requires layout facts; executable layout regions were removed.
// XFAIL: *
// RUN: fcc compile --stage ir -o - %s | filecheck %s

struct mixed {
    float value;
    int tag;
};

float values[3] = {1.0f};
struct mixed item = {1.5f, 7};

// CHECK: global @values align 4 bytes [0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0]
// CHECK: global @item align 4 bytes [0, 0, 192, 63, 7, 0, 0, 0]
