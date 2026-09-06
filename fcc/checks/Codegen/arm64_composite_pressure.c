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

// The callee reads the composite out of the caller's outgoing area, which sits
// just above its own 16-byte frame. The frame exists only to hold the
// parameter's local copy: the two halves are stored to it and never read back
// (the return value is the second half, already in a register), but the stores
// sit on the function's one conservative memory chain and that chain reaches
// the return, so they stay. Per-object chains let the block-based mid-end drop
// the unread copy along with the frame, and the loads were at `[sp, 0]` and
// `[sp, 8]`.
// ASM-LABEL: pressured:
// ASM: sub sp, sp, 16
// ASM-NEXT: ldr {{x[0-9]+}}, [sp, 16]
// ASM-NEXT: ldr x0, [sp, 24]
// ASM: str {{x[0-9]+}}, [{{x[0-9]+}}, 0]
// ASM: str x0, [{{x[0-9]+}}, 8]
// ASM: add sp, sp, 16
// ASM-NEXT: ret x30
// ASM-LABEL: call_pressured:
// ASM: str {{x[0-9]+}}, [sp, 0]
// ASM: str {{x[0-9]+}}, [sp, 8]
// ASM: bl sink_pressured
