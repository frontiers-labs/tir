// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Mixed {
    double fp;
    long integer;
};

long consume_mixed(struct Mixed value) {
    return value.integer + (long) value.fp;
}

struct Mixed make_mixed(double fp, long integer) {
    struct Mixed result = {fp, integer};
    return result;
}

// CHECK: %{{[0-9]+}} = func.func @consume_mixed(%{{[0-9]+}}: !tuple<!i64, !i64>) -> !i64 {
// CHECK: %{{[0-9]+}} = func.func @make_mixed(
// CHECK-SAME: ) -> !tuple<!i64, !i64> {
// CHECK: make_tuple {{.*}} : !tuple<!i64, !i64>

// ASM-LABEL: consume_mixed:
// ASM: str x0
// ASM: str x1
// A mixed struct is no HFA: the double parameter arrives in d0 but leaves as
// the low half of the x0/x1 return pair, with the long parameter in x1. Which
// register carries which field is the ABI fact; whether x1 is reloaded from the
// slot or forwarded from x0 is a scheduling choice.
// ASM-LABEL: make_mixed:
// ASM-DAG: str d0, [{{x[0-9]+}}, 0]
// ASM-DAG: ldr x0, [{{x[0-9]+}}, 0]
// ASM-DAG: {{(ldr|orr) x1}}
