// RUN: fcc compile -O2 --march riscv64 --mabi lp64d --stage ir -o - %s | filecheck %s
// RUN: fcc compile -O2 --march riscv64 --mabi lp64d --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Large {
    long values[3];
};

struct Large make_large(long a, long b, long c) {
    struct Large result = {{a, b, c}};
    return result;
}

extern struct Large sink_large(long a, long b, long c);

struct Large forward_large(long a, long b, long c) {
    return sink_large(a, b, c);
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @make_large(
// CHECK-SAME: %[[MAKE_DEST:[0-9]+]]: !ptr.p, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64) result_address {
// CHECK: ptr.memcpy %[[MAKE_DEST]]
// CHECK: func.return
// CHECK-LABEL: %{{[0-9]+}} = func.func @forward_large(
// CHECK-SAME: %[[FORWARD_DEST:[0-9]+]]: !ptr.p, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64) result_address {
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: func.call %{{[0-9]+}}(%[[TEMP]], %{{[0-9]+}}, %{{[0-9]+}}, %{{[0-9]+}}
// CHECK-SAME: result_address
// CHECK: ptr.memcpy %[[FORWARD_DEST]], %[[TEMP]]
// CHECK: func.return

// ASM-LABEL: make_large:
// ASM: sd x11,
// ASM-LABEL: forward_large:
// ASM: c.mv x10,
// ASM: jal x1, sink_large
// ASM: jal x1, memcpy
