// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Large {
    long values[3];
};

struct Large make_large(long a, long b, long c) {
    struct Large result = {{a, b, c}};
    return result;
}

struct Large forward_large(long a, long b, long c) {
    return make_large(a, b, c);
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @make_large(%[[MAKE_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) result_address {
// CHECK: ptr.memcpy %[[MAKE_DEST]]
// CHECK: func.return
// CHECK-LABEL: %{{[0-9]+}} = func.func @forward_large(%[[FORWARD_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) result_address {
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: func.call %{{[0-9]+}}(%[[TEMP]]
// CHECK-SAME: ) result_address
// CHECK: ptr.memcpy %[[FORWARD_DEST]], %[[TEMP]]
// CHECK: func.return

// ASM-LABEL: make_large:
// ASM: bl memcpy
// ASM-LABEL: forward_large:
// ASM: bl make_large
// ASM: bl memcpy
