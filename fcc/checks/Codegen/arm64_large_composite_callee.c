// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Large {
    long values[3];
};

long consume_large(struct Large value) {
    return value.values[2];
}

// CHECK: func.func @consume_large(%[[VALUE:[0-9]+]]: !ptr.p) -> !i64 {
// CHECK-NOT: ptr.alloca {size = 24
// CHECK: cir.get_member %[[VALUE]]

// ASM-LABEL: consume_large:
// ASM: ldr x0, [x0, 16]
