// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march arm64 --mabi aapcs64 --stage asm -o - %s | filecheck %s --check-prefix=ASM

struct Pair {
    double left;
    double right;
};

struct Quad {
    double values[4];
};

double consume_pair(struct Pair pair) {
    return pair.left + pair.right;
}

double call_consume_pair(struct Pair pair) {
    return consume_pair(pair);
}

struct Quad make_quad(double a, double b, double c, double d) {
    struct Quad result = {{a, b, c, d}};
    return result;
}

// CHECK: func.func @consume_pair(%{{[0-9]+}}: !tuple<!f64, !f64>) -> !f64 {
// CHECK: func.func @call_consume_pair(%{{[0-9]+}}: !tuple<!f64, !f64>) -> !f64 {
// CHECK: make_tuple {{.*}} : !tuple<!f64, !f64>
// CHECK: func.call @consume_pair({{.*}} : !tuple<!f64, !f64>) -> !f64
// CHECK: func.func @make_quad(
// CHECK-SAME: ) -> !tuple<!f64, !f64, !f64, !f64> {
// CHECK: make_tuple {{.*}} : !tuple<!f64, !f64, !f64, !f64>

// ASM-LABEL: consume_pair:
// ASM: str d0
// ASM: str d1
// ASM-LABEL: call_consume_pair:
// ASM: bl consume_pair
// A four-double HFA is passed and returned in d0-d3, element i in d(i): the
// stores pin the parameter-to-field mapping, the loads pin the field-to-result
// mapping. d3 needs no reload when it still holds element 3.
// ASM-LABEL: make_quad:
// ASM-DAG: str d0, [{{x[0-9]+}}, 0]
// ASM-DAG: str d1, [{{x[0-9]+}}, 8]
// ASM-DAG: str d2, [{{x[0-9]+}}, 16]
// ASM-DAG: str d3, [{{x[0-9]+}}, 24]
// ASM-DAG: ldr d0, [{{x[0-9]+}}, 0]
// ASM-DAG: ldr d1, [{{x[0-9]+}}, 8]
// ASM-DAG: ldr d2, [{{x[0-9]+}}, 16]
