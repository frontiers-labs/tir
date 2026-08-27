// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Mixed {
    double fp;
    long integer;
};

long pressured(long a, long b, long c, long d, long e, long f, long g,
               struct Mixed value) {
    return value.integer;
}

long call_pressured(long a, long b, long c, long d, long e, long f, long g,
                    struct Mixed value) {
    return pressured(a, b, c, d, e, f, g, value);
}

// CHECK: %{{[0-9]+}} = func.func @pressured(
// CHECK-SAME: !i64, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64,
// CHECK-SAME: !i64, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64,
// CHECK-SAME: !i64, %{{[0-9]+}}: !tuple<!i64, !i64>) -> !i64 {
// CHECK: %{{[0-9]+}} = func.func @call_pressured(
// CHECK-SAME: !tuple<!i64, !i64>) -> !i64 {

// The callee reads the composite out of the caller's outgoing area, which its
// own stack pointer points at: it allocates no frame of its own, so the two
// halves sit at the bottom of it.
// ASM-LABEL: pressured:
// ASM: ldr {{x[0-9]+}}, [sp, 0]
// ASM: ldr {{x[0-9]+}}, [sp, 8]
// ASM-LABEL: call_pressured:
// ASM: str {{x[0-9]+}}, [sp, 0]
// ASM: str {{x[0-9]+}}, [sp, 8]
// ASM: bl pressured
