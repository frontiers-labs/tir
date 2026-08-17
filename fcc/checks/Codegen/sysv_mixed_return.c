// RUN: fcc compile --march x86_64 --mabi sysv --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --mabi sysv --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Mixed {
    double fp;
    long integer;
};

struct Mixed make_mixed(double fp, long integer) {
    struct Mixed result = {fp, integer};
    return result;
}

// CHECK: func.func @make_mixed(
// CHECK-SAME: ) -> !tuple<!f64, !i64> {
// CHECK: make_tuple {{.*}} : !tuple<!f64, !i64>

// ASM-LABEL: make_mixed:
// ASM: movsd xmm0,
// ASM: mov rax,
