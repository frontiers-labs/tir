// RUN: fcc compile --march x86_64 --mabi sysv --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --mabi sysv --stage asm -o - %s | filecheck %s --check-prefix=ASM

union Bits {
    double fp;
    long integer;
};

long read_bits(union Bits value) {
    return value.integer;
}

// CHECK: %{{[0-9]+}} = func.func @read_bits(%{{[0-9]+}}: !i64) -> !i64 {

// ASM-LABEL: read_bits:
// ASM: mov {{.*}}, rdi
