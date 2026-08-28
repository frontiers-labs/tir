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

// CHECK: %{{[0-9]+}} = func.func @scalar_value(%{{[0-9]+}}: !f64) -> !f64 {
// CHECK: %{{[0-9]+}} = func.func @pair_sum(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {

// A one-field aggregate is one register: the slot the frontend spelled for it is
// local, whole and of one type, so construction carries its value and the
// function stores nothing.
// ASM-LABEL: scalar_value:
// ASM-NOT: fsd
// ASM-LABEL: pair_sum:
// ASM: fsd f10, 0({{.*}})
// ASM: fsd f11, 8({{.*}})

// SOFT: %{{[0-9]+}} = func.func @scalar_value(%{{[0-9]+}}: !i64) -> !f64 {
// SOFT: %{{[0-9]+}} = func.func @pair_sum(%{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64) -> !f64 {
