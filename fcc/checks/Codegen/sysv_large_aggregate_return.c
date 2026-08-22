// RUN: fcc compile --march x86_64 --mabi sysv --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --mabi sysv --stage asm -o - %s | filecheck %s --check-prefix=ASM

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
// CHECK-SAME: ) -> !ptr.p result_address {
// CHECK: ptr.memcpy %[[MAKE_DEST]]
// CHECK: func.return %[[MAKE_DEST]]
// CHECK-LABEL: %{{[0-9]+}} = func.func @forward_large(%[[FORWARD_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) -> !ptr.p result_address {
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: %{{[0-9]+}} = func.call %{{[0-9]+}}(%[[TEMP]]
// CHECK-SAME: ) -> !ptr.p result_address
// CHECK: ptr.memcpy %[[FORWARD_DEST]], %[[TEMP]]
// CHECK: func.return %[[FORWARD_DEST]]

// ASM-LABEL: make_large:
// ASM: call memcpy
// ASM: mov rax,
// ASM-LABEL: forward_large:
// ASM: call make_large
// ASM: call memcpy
// ASM: mov rax,
