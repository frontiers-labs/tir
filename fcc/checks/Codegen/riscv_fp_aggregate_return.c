// RUN: fcc compile --march riscv64 --mabi lp64d --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march riscv64 --mabi lp64d --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Scalar {
    double value;
};

struct Pair {
    double left;
    double right;
};

struct Scalar make_scalar(double value) {
    struct Scalar result = {value};
    return result;
}

struct Pair make_pair(double left, double right) {
    struct Pair result = {left, right};
    return result;
}

struct Pair external_pair(double, double);

double call_external_pair(void) {
    struct Pair result = external_pair(1.0, 2.0);
    return result.left + result.right;
}

// CHECK: func.func @make_scalar(%{{[0-9]+}}: !f64) -> !f64 {
// CHECK: func.return %{{[0-9]+}}
// CHECK: func.func @make_pair(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !tuple<!f64, !f64> {
// CHECK: %[[PAIR:[0-9]+]] = make_tuple %{{[0-9]+}}, %{{[0-9]+}} : !tuple<!f64, !f64>
// CHECK: func.return %[[PAIR]]
// CHECK: func.declare @external_pair(!f64, !f64) -> !tuple<!f64, !f64>
// CHECK: func.func @call_external_pair() -> !f64 {
// CHECK: %[[CALL:[0-9]+]] = func.call @external_pair({{.*}}) -> !tuple<!f64, !f64>
// CHECK: tuple_get %[[CALL]] {index = 0} : !f64
// CHECK: tuple_get %[[CALL]] {index = 1} : !f64

// A two-double aggregate travels entirely in the FP argument/result registers:
// left in f10, right in f11, both on the way in and on the way out. f11 needs
// no reload when it still holds the right element.
// ASM-LABEL: make_pair:
// ASM-DAG: fsd f10, 0({{.*}})
// ASM-DAG: fsd f11, 8({{.*}})
// ASM-DAG: fld f10, 0({{.*}})
// ASM-LABEL: call_external_pair:
// ASM: jal x1, external_pair
// ASM: fsd f{{[0-9]+}}, 0({{.*}})
// ASM: fsd f{{[0-9]+}}, 8({{.*}})
