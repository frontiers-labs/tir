// RUN: fcc compile --march x86_64 --mabi sysv --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --mabi sysv --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Mixed {
    double fp;
    long integer;
};

long consume_mixed(struct Mixed value) {
    return value.integer;
}

// CHECK: %{{[0-9]+}} = func.func @consume_mixed(%{{[0-9]+}}: !tuple<!f64, !i64>) -> !i64 {

// ASM-LABEL: consume_mixed:
// ASM: movsd {{.*}}, xmm0
// ASM: mov {{.*}}, rdi
