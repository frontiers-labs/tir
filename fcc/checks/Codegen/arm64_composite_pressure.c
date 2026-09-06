// RUN: fcc compile -O2 --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile -O2 --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Mixed {
    double fp;
    long integer;
};

long pressured(long a, long b, long c, long d, long e, long f, long g,
               struct Mixed value) {
    return value.integer;
}

extern long sink_pressured(long a, long b, long c, long d, long e, long f,
                           long g, struct Mixed value);

long call_pressured(long a, long b, long c, long d, long e, long f, long g,
                    struct Mixed value) {
    return sink_pressured(a, b, c, d, e, f, g, value);
}

// CHECK: %{{[0-9]+}} = func.func @pressured(
// CHECK-SAME: !i64, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64,
// CHECK-SAME: !i64, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64,
// CHECK-SAME: !i64, %{{[0-9]+}}: !tuple<!i64, !i64>) -> !i64 {
// CHECK: %{{[0-9]+}} = func.func @call_pressured(
// CHECK-SAME: !tuple<!i64, !i64>) -> !i64 {

// The callee reads the composite halves straight out of the caller's outgoing
// area and returns the second. The parameter's local copy is written and never
// read, so the mid-end removes it along with the frame that held it.
// ASM-LABEL: pressured:
// ASM-NOT: sub sp
// ASM: ldr x0, [sp, 0]
// ASM-NEXT: ldr x0, [sp, 8]
// ASM-NEXT: ret x30
// ASM-LABEL: call_pressured:
// ASM: str {{x[0-9]+}}, [sp, 0]
// ASM: str {{x[0-9]+}}, [sp, 8]
// ASM: bl sink_pressured
