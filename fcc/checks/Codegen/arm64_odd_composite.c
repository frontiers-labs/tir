// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Three {
    char bytes[3];
};

long consume_three(struct Three value) {
    return sizeof(value);
}

long call_three(struct Three *value) {
    return consume_three(*value);
}

struct Three make_three(struct Three *value) {
    return *value;
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @consume_three(%{{[0-9]+}}: !i64) -> !i64 {
// CHECK: ptr.alloca {size = 8, align = 8}
// CHECK-LABEL: %{{[0-9]+}} = func.func @call_three(
// CHECK: ptr.alloca {size = 8, align = 8}
// CHECK: constant {value = 0} : !i64
// CHECK: ptr.store
// CHECK: constant {value = 3} : !i64
// CHECK: ptr.memcpy
// CHECK: ptr.load {{.*}} : !i64
// CHECK: func.call %{{[0-9]+}}({{.*}} : !i64) -> !i64
// CHECK-LABEL: %{{[0-9]+}} = func.func @make_three(
// CHECK-SAME: ) -> !i64 {
// CHECK: ptr.alloca {size = 8, align = 8}
// CHECK: ptr.memcpy
// CHECK: ptr.load {{.*}} : !i64

// ASM-LABEL: call_three:
// ASM: bl memcpy
// ASM: bl consume_three
