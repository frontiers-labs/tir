// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s

struct Large {
    long values[3];
};

struct Large make_large(long a, long b, long c);

long first(long a, long b, long c) {
    return make_large(a, b, c).values[0];
}

// CHECK: func.declare @make_large(!ptr.p, !i64, !i64, !i64) -> !unit
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: func.call @make_large(%[[TEMP]]
// CHECK-SAME: ) result_address
// CHECK: ptr.ptradd %[[TEMP]]
