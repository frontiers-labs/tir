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

// A three-byte struct travels in one i64: the caller zeroes an 8-byte slot,
// copies the three bytes into it and loads the word in that order. The slot's
// address reaches the copy, so it is not private: every write joins the world
// chain and splits back off it. The callee returns the word the same way.

// CHECK-LABEL: %{{[0-9]+}} = func.func @consume_three(%{{[0-9]+}}: !i64) -> !i64 {
// CHECK: ptr.alloca {size = 8, align = 8}
// CHECK-LABEL: %{{[0-9]+}} = func.func @call_three(
// CHECK: ptr.alloca {size = 8, align = 8}
// CHECK: %[[ZERO:[0-9]+]] = constant {value = 0} : !i64
// CHECK: %[[SIZE:[0-9]+]] = constant {value = 3} : !i64
// CHECK: %[[CLEAR:[0-9]+]] = ptr.store %[[ZERO]], %[[SLOT:[0-9]+]]
// CHECK-NEXT: %[[COPY:[0-9]+]] = ptr.memcpy %[[SLOT]], %{{[0-9]+}}, %[[SIZE]] state(%[[CLEAR]])
// CHECK-NEXT: %[[WORD:[0-9]+]], %[[LOAD:[0-9]+]] = ptr.load %[[SLOT]] state(%[[COPY]]) : !i64
// CHECK-NEXT: %{{[0-9]+}}, %{{[0-9]+}} = func.call %{{[0-9]+}}(%[[WORD]] : !i64) -> !i64 state(%[[LOAD]])
// CHECK-LABEL: %{{[0-9]+}} = func.func @make_three(
// CHECK-SAME: ) -> !i64 {
// CHECK: ptr.alloca {size = 8, align = 8}
// CHECK: ptr.memcpy
// CHECK: %[[RET_COPY:[0-9]+]] = ptr.memcpy %[[RET:[0-9]+]]
// CHECK-NEXT: %[[RET_WORD:[0-9]+]], %[[RET_LOAD:[0-9]+]] = ptr.load %[[RET]] state(%[[RET_COPY]]) : !i64
// CHECK-NEXT: %[[RET_OUT:[0-9]+]] = state.join state(%{{[0-9]+}}, %[[RET_LOAD]])
// CHECK-NEXT: -> %[[RET_WORD]], %[[RET_OUT]]

// ASM-LABEL: call_three:
// ASM: bl memcpy
// ASM: bl consume_three
