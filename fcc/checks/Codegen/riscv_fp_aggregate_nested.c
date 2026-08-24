// RUN: fcc compile --march riscv64 --mabi lp64d --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march riscv64 --mabi lp64d --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Element {
    double value;
};

struct Nested {
    struct Element values[2];
};

double consume_nested(struct Nested value) {
    return value.values[0].value + value.values[1].value;
}
struct Nested produce_nested(void);

// CHECK: %{{[0-9]+}} = func.func @consume_nested(%{{[0-9]+}}: !f64, %{{[0-9]+}}: !f64) -> !f64 {
// CHECK: %{{[0-9]+}} = func.declare @produce_nested() -> !tuple<!f64, !f64>

// ASM-LABEL: consume_nested:
// ASM: fsd f10, 0({{.*}})
// ASM: fsd f11, 8({{.*}})
