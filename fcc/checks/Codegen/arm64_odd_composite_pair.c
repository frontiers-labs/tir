// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Twelve {
    int words[3];
};

long consume_twelve(struct Twelve value) {
    return sizeof(value);
}

long call_twelve(struct Twelve *value) {
    return consume_twelve(*value);
}

struct Twelve make_twelve(struct Twelve *value) {
    return *value;
}

// CHECK-LABEL: func.func @consume_twelve(%{{[0-9]+}}: !tuple<!i64, !i64>) -> !i64 {
// CHECK: ptr.alloca {size = 16, align = 8}
// CHECK-LABEL: func.func @call_twelve(
// CHECK: ptr.alloca {size = 16, align = 8}
// CHECK-COUNT-2: constant {value = 0} : !i64
// CHECK: constant {value = 12} : !i64
// CHECK: ptr.memcpy
// CHECK: make_tuple {{.*}} : !tuple<!i64, !i64>
// CHECK: func.call @consume_twelve({{.*}} : !tuple<!i64, !i64>) -> !i64
// CHECK-LABEL: func.func @make_twelve(
// CHECK-SAME: ) -> !tuple<!i64, !i64> {
// CHECK: ptr.alloca {size = 16, align = 8}
// CHECK: ptr.memcpy
// CHECK: make_tuple {{.*}} : !tuple<!i64, !i64>

// ASM-LABEL: call_twelve:
// ASM: bl memcpy
// ASM: bl consume_twelve
