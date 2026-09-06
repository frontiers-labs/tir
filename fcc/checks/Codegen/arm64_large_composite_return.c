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

// A struct too large for registers is returned through a result address: the
// callee copies into its leading pointer parameter and the function's own
// result is the copy's dependency; the forwarding caller passes a temporary,
// then copies it into its own result address after the call.

// CHECK-LABEL: %{{[0-9]+}} = func.func @make_large(%[[MAKE_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) result_address {
// CHECK: | %[[MAKE_COPY:[0-9]+]] = ptr.memcpy %[[MAKE_DEST]]
// CHECK-NEXT: -> | %[[MAKE_COPY]]
// CHECK-LABEL: %{{[0-9]+}} = func.func @forward_large(%[[FORWARD_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) result_address {
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: | %[[CALL:[0-9]+]] = func.call %{{[0-9]+}}(%[[TEMP]]
// CHECK-SAME: ) result_address
// CHECK: | %[[FORWARD_COPY:[0-9]+]] = ptr.memcpy %[[FORWARD_DEST]], %[[TEMP]], %{{[0-9]+}} | %[[CALL]]
// CHECK-NEXT: -> | %[[FORWARD_COPY]]

// ASM-LABEL: make_large:
// ASM: bl memcpy
// ASM-LABEL: forward_large:
// ASM: bl make_large
// ASM: bl memcpy
