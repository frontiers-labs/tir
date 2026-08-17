// RUN: fcc compile --stage ir -o - %s | filecheck %s

int advance(int limit) {
    int value;
    for (value = 0; value < limit; value += 2) {
    }
    return value;
}

// CHECK: scf.while {
// CHECK: cmpi {{.*}} {predicate = "slt"}
// CHECK: addi
// CHECK: ptr.store
// CHECK: scf.condition
