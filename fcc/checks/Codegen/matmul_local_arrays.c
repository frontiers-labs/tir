// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-local.tir %s
// RUN: tir opt --pass func.func(promote,thread-state,instcombine,affine) /tmp/fcc-matmul-local.tir | filecheck %s
// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s --check-prefix=ASM

// The affine view's own case: three local arrays are three memories, every
// subscript is a form over the counters, and the only pair is `C[i][j]` against
// itself. Nothing has to be assumed about aliasing for the nest to be
// reordered, and it is: `k` moves out of the innermost position so that the
// read of `b` walks a row rather than a column. Measured on the corpus, that
// order runs the kernel in 10 % fewer cycles; every tiling of it ran slower
// (the numbers are in Spec 07's amendments), and the model buys none here.
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
// CHECK: scf.for
// CHECK: scf.for
// CHECK: scf.for {{.*}} iter_args(%[[I:[0-9]+]] = {{.*}} -> !i32 {
// CHECK-NEXT: scf.for {{.*}} iter_args(%[[K:[0-9]+]] = {{.*}} -> !i32 {
// CHECK-NEXT: scf.for {{.*}} iter_args(%[[J:[0-9]+]] = {{.*}} -> !i32 {
// CHECK-NEXT: extsi %[[I]]
// CHECK: extsi %[[J]]
// CHECK: extsi %[[K]]

// ASM: matmul_local_arrays:
