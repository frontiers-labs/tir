// RUN: fcc compile --march riscv64 --mabi lp64d --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march riscv64 --mabi lp64d --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct FloatInt {
    double value;
    long tag;
};

struct IntFloat {
    long tag;
    double value;
};

long consume_float_int(struct FloatInt input) {
    return input.tag;
}

struct IntFloat make_int_float(long tag, double value) {
    struct IntFloat result = {tag, value};
    return result;
}

// CHECK: func.func @consume_float_int(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !i64) -> !i64 {
// CHECK: func.func @make_int_float(%{{[0-9]+}}: !i64, %{{[0-9]+}}: !f64) -> !tuple<!i64, !f64> {
// CHECK: make_tuple %{{[0-9]+}}, %{{[0-9]+}} : !tuple<!i64, !f64>

// ASM-LABEL: consume_float_int:
// ASM: fsd f10, 0({{.*}})
// ASM: sd x10, 8({{.*}})
// The mirrored aggregate keeps the same banks in the reversed field order: tag
// in x10, value in f10, both on the way in and on the way out. f10 needs no
// reload when it still holds the value.
// ASM-LABEL: make_int_float:
// ASM-DAG: sd x10, 0({{.*}})
// ASM-DAG: fsd f10, 8({{.*}})
// ASM-DAG: ld x10, 0({{.*}})
