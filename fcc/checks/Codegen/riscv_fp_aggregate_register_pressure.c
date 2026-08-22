// RUN: fcc compile --march riscv64 --mabi lp64d --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march riscv64 --mabi lp64d --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Pair {
    double left;
    double right;
};

struct Mixed {
    double value;
    long tag;
};

double consume_after_seven_doubles(
    double a,
    double b,
    double c,
    double d,
    double e,
    double f,
    double g,
    struct Pair pair
) {
    return pair.left + pair.right;
}

long consume_after_eight_longs(
    long a,
    long b,
    long c,
    long d,
    long e,
    long f,
    long g,
    long h,
    struct Mixed mixed
) {
    return mixed.tag;
}

// CHECK: %{{[0-9]+}} = func.func @consume_after_seven_doubles(
// CHECK-SAME: %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64) -> !f64
// CHECK: %{{[0-9]+}} = func.func @consume_after_eight_longs(
// CHECK-SAME: %{{[0-9]+}}: !i64, %{{[0-9]+}}: !i64) -> !i64

// ASM-LABEL: consume_after_seven_doubles:
// ASM: sd x10, {{[0-9]+}}({{.*}})
// ASM: sd x11, {{[0-9]+}}({{.*}})
