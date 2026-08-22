// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Aligned {
    long double value;
};

long consume_aligned(long head, struct Aligned value) {
    return head + sizeof(value);
}

long call_aligned(long head, struct Aligned *value) {
    return consume_aligned(head, *value);
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @consume_aligned(
// CHECK-SAME: %{{[0-9]+}}: !i64, %{{[0-9]+}}: !tuple<!i64, !i64>) -> !i64 argument_alignments [1, 16] {
// CHECK-LABEL: %{{[0-9]+}} = func.func @call_aligned(
// CHECK: func.call %{{[0-9]+}}({{.*}} : !i64, !tuple<!i64, !i64>) -> !i64 argument_alignments [1, 16]

// ASM-LABEL: consume_aligned:
// ASM: str x2
// ASM: str x3
// ASM-LABEL: call_aligned:
// ASM: orr x2,
// ASM: orr x3,
// ASM: bl consume_aligned
