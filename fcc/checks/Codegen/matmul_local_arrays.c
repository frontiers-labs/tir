// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-local.tir %s
// RUN: tir opt --pass func.func(promote-nodes,verify-deps,instcombine-nodes,affine) /tmp/fcc-matmul-local.tir | filecheck %s
// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s --check-prefix=ASM

// The affine view's own case: three local arrays are three memories, every
// subscript is a form over the counters, and the only pair is `C[i][j]` against
// itself. With per-object chains the nest was reordered, `k` out of the
// innermost position so the read of `b` walks a row (10 % fewer cycles on the
// corpus; every tiling ran slower). Today the nest stays `i, j, k`: the `j`
// counter is stored to its slot in the `j` loop and reloaded inside the `k`
// loop, promote-nodes does not forward a port across that loop boundary, so
// the `b` subscript is built from a `ptr.load` and the affine view sees a
// memory read where it needs a form over the counters. It refuses the
// interchange. This pins that reload and the unmoved order.
//
// `int` rather than `float` because fcc's float lowering is broken today: a
// `float` load comes back as `!i64` and the arithmetic on it is integer
// arithmetic (`float f(float a, float b) { return a * b; }` reproduces it), so a
// float kernel here would measure a miscompile.

void matmul_local_arrays(int *out)
{
    int a[64][64];
    int b[64][64];
    int c[64][64];

    for (int i = 0; i < 64; i++)
        for (int j = 0; j < 64; j++)
            c[i][j] = i + j;

    for (int i = 0; i < 64; i++)
        for (int j = 0; j < 64; j++)
            for (int k = 0; k < 64; k++)
                c[i][j] += a[i][k] * b[k][j];

    for (int i = 0; i < 64; i++)
        out[i] = c[i][i];
}

// CHECK-LABEL: func.func @matmul_local_arrays
// CHECK: scf.for2
// CHECK: scf.for2
// CHECK: scf.for2 {{.*}} (%[[I:[0-9]+]] = {{.*}}) {
// CHECK: scf.for2 {{.*}} (%[[J:[0-9]+]] = {{.*}}) {
// CHECK: ptr.store %[[J]], %[[JSLOT:[0-9]+]] |
// CHECK: scf.for2 {{.*}} (%[[K:[0-9]+]] = {{.*}}) {
// CHECK: extsi %[[I]]
// CHECK-NEXT: %[[JL:[0-9]+]] | %{{[0-9]+}} = ptr.load %[[JSLOT]] |
// CHECK-NEXT: extsi %[[JL]]
// CHECK-NEXT: extsi %[[K]]

// ASM: matmul_local_arrays:
