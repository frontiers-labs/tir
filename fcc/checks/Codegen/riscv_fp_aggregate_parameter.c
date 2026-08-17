// RUN: fcc compile --march riscv64 --mabi lp64d --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march riscv64 --mabi lp64d --stage asm -o - %s | filecheck %s --check-prefix=ASM
// RUN: fcc compile --march riscv64 --mabi lp64 --stage ir -o - %s | filecheck %s --check-prefix=SOFT

struct Scalar {
    double value;
};

struct Pair {
    double left;
    double right;
};

double scalar_value(struct Scalar value) {
    return value.value;
}

double pair_sum(struct Pair pair) {
    return pair.left + pair.right;
}

// CHECK: func.func @scalar_value(%{{[0-9]+}}: !f64) -> !f64 {
// CHECK: func.func @pair_sum(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {

// ASM-LABEL: scalar_value:
// ASM: fsd f10, 0({{.*}})
// ASM-LABEL: pair_sum:
// ASM: fsd f10, 0({{.*}})
// ASM: fsd f11, 8({{.*}})

// SOFT: func.func @scalar_value(%{{[0-9]+}}: !i64) -> !f64 {
// SOFT: func.func @pair_sum(%{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64) -> !f64 {
