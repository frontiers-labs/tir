// RUN: fcc compile -O2 --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile -O2 --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Quad {
    double values[4];
};

double pressured(double a, double b, double c, double d, double e, double f,
                 struct Quad quad) {
    return a + quad.values[0];
}

extern double sink_pressured(double a, double b, double c, double d, double e,
                             double f, struct Quad quad);

double call_pressured(double a, double b, double c, double d, double e, double f,
                      struct Quad quad) {
    return sink_pressured(a, b, c, d, e, f, quad);
}

// CHECK: %{{[0-9]+}} = func.func @pressured(
// CHECK-SAME: !f64, %{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64,
// CHECK-SAME: !f64, %{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64,
// CHECK-SAME: !tuple<!f64, !f64, !f64, !f64>) -> !f64 {
// CHECK: %{{[0-9]+}} = func.func @call_pressured(
// CHECK-SAME: !tuple<!f64, !f64, !f64, !f64>) -> !f64 {
// CHECK: make_tuple {{.*}} : !tuple<!f64, !f64, !f64, !f64>
// CHECK: func.call %{{[0-9]+}}({{.*}} : !f64, !f64, !f64, !f64, !f64, !f64, !tuple<!f64, !f64, !f64, !f64>) -> !f64

// ASM-LABEL: pressured:
// ASM: ldr {{d[0-9]+}}, [sp, 32]
// ASM: ldr {{d[0-9]+}}, [sp, 40]
// ASM: ldr {{d[0-9]+}}, [sp, 48]
// ASM: ldr {{d[0-9]+}}, [sp, 56]
// ASM-LABEL: call_pressured:
// ASM: str {{d[0-9]+}}, [sp, 0]
// ASM: str {{d[0-9]+}}, [sp, 8]
// ASM: str {{d[0-9]+}}, [sp, 16]
// ASM: str {{d[0-9]+}}, [sp, 24]
// ASM: bl sink_pressured
