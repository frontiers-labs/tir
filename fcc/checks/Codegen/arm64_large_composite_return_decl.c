// RUN: fcc compile --march arm64 --mabi aapcs64 --stage ir -o - %s | filecheck %s

struct Large {
    long values[3];
};

struct Large make_large(long a, long b, long c);

long first(long a, long b, long c) {
    return make_large(a, b, c).values[0];
}

// The declared callee returns its struct through a result address: the
// caller allocates the temporary, passes it as the leading pointer argument,
// and the field read is a load from that temporary chained on the call.

// CHECK: %{{[0-9]+}} = func.declare @make_large(!ptr.p, !i64, !i64, !i64) -> !unit
// CHECK: %[[TEMP:[0-9]+]] = ptr.alloca {size = 24, align = 8}
// CHECK: %[[BASE:[0-9]+]] = ptr.ptradd %[[TEMP]]
// CHECK: %[[FIELD:[0-9]+]] = ptr.ptradd %[[BASE]]
// CHECK: | %[[CALL:[0-9]+]] = func.call %{{[0-9]+}}(%[[TEMP]]
// CHECK-SAME: ) result_address
// CHECK: ptr.load %[[FIELD]] | %[[CALL]]
