// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A `+=` step counts like any other: the constant it adds becomes the loop's
// step, and a bound read from a parameter's slot is read once before the loop,
// off the same state as the initial counter.

int advance(int limit) {
    int value;
    for (value = 0; value < limit; value += 2) {
    }
    return value;
}

// CHECK: %[[ST:[0-9]+]] = constant {value = 2} : !i32
// CHECK: %[[LB:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %[[S:[0-9]+]] : !i32
// CHECK-NEXT: %[[UB:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %[[S]] : !i32
// CHECK: scf.for2 %{{[0-9]+}} = %[[LB]] to %[[UB]] step %[[ST]] (
