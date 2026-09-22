// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Large {
    long values[3];
};

long consume_large(struct Large value);

long call_large(struct Large *value) {
    return consume_large(*value);
}

// CHECK: %{{[0-9]+}} = func.declare @consume_large(!ptr.p) -> !i64
// CHECK: ptr.alloca {size = 24, align = 8}
// CHECK: %[[SIZE:[0-9]+]] = constant {value = 24} : !i64
// CHECK: ptr.memcpy %[[COPY:[0-9]+]], %{{[0-9]+}}, %[[SIZE]]
// CHECK: func.call %{{[0-9]+}}(%[[COPY]] : !ptr.p) -> !i64

// ASM-LABEL: call_large:
// ASM-NOT: bl memcpy
// ASM: ldr {{.*}}, [{{.*}}, 0]
// ASM: str {{.*}}, [{{.*}}, 0]
// ASM: ldr {{.*}}, [{{.*}}, 8]
// ASM: str {{.*}}, [{{.*}}, 8]
// ASM: ldr {{.*}}, [{{.*}}, 16]
// ASM: str {{.*}}, [{{.*}}, 16]
// ASM: bl consume_large
