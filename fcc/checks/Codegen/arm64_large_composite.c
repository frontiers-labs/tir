// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Large {
    long values[3];
};

long consume_large(struct Large value);

long call_large(struct Large *value) {
    return consume_large(*value);
}

// CHECK: func.declare @consume_large(!ptr.p) -> !i64
// CHECK: ptr.alloca {size = 24, align = 8}
// CHECK: %[[SIZE:[0-9]+]] = constant {value = 24} : !i64
// CHECK: ptr.memcpy %[[COPY:[0-9]+]], %{{[0-9]+}}, %[[SIZE]]
// CHECK: func.call @consume_large(%[[COPY]] : !ptr.p) -> !i64

// ASM-LABEL: call_large:
// ASM: bl memcpy
// ASM: bl consume_large
