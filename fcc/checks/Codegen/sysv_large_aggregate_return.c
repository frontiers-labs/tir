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

// A struct too large for registers is returned through a result address,
// which SysV also hands back in rax: the callee copies into its leading
// pointer parameter and returns that pointer on the copy's chain; the
// forwarding caller passes a temporary, copies it into its own result address
// after the call, and returns the address the same way.

// CHECK-LABEL: %{{[0-9]+}} = func.func @make_large(%[[MAKE_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) -> !ptr.p result_address {
// CHECK: | %[[MAKE_COPY:[0-9]+]] = ptr.memcpy %[[MAKE_DEST]]
// CHECK-NEXT: -> %[[MAKE_DEST]] | %[[MAKE_COPY]]
// CHECK-LABEL: %{{[0-9]+}} = func.func @forward_large(%[[FORWARD_DEST:[0-9]+]]: !ptr.p,
// CHECK-SAME: ) -> !ptr.p result_address {
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: %{{[0-9]+}} | %[[CALL:[0-9]+]] = func.call %{{[0-9]+}}(%[[TEMP]]
// CHECK-SAME: ) -> !ptr.p result_address
// CHECK: | %[[FORWARD_COPY:[0-9]+]] = ptr.memcpy %[[FORWARD_DEST]], %[[TEMP]], %{{[0-9]+}} | %[[CALL]]
// CHECK-NEXT: -> %[[FORWARD_DEST]] | %[[FORWARD_COPY]]

// ASM-LABEL: make_large:
// ASM: call memcpy
// ASM: mov rax,
// ASM-LABEL: forward_large:
// ASM: call make_large
// ASM: call memcpy
// ASM: mov rax,
