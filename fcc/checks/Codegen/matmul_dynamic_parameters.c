// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-dynamic.tir %s
// RUN: tir opt --pass func.func(promote-nodes,verify-deps,instcombine-nodes,affine) /tmp/fcc-matmul-dynamic.tir | filecheck %s --check-prefix=AFFINE

// A runtime row length prevents integer affine dependence analysis from
// reordering these loops. Modular recurrences can still reduce k*n+j while
// preserving the original i, j, k order and memory dependencies.

void matmul_dynamic_parameters(int *a, int *b, int *c, int n)
{
    for (int i = 0; i < n; i++)
        for (int j = 0; j < n; j++)
            for (int k = 0; k < n; k++)
                c[i * n + j] += a[i * n + k] * b[k * n + j];
}

// CHECK: matmul_dynamic_parameters:

// AFFINE-LABEL: func.func @matmul_dynamic_parameters({{.*}}%[[N:[0-9]+]]: !i32)
// AFFINE: scf.for %[[I:[0-9]+]] =
// AFFINE: scf.for %[[J:[0-9]+]] =
// AFFINE: scf.for %[[K:[0-9]+]] = {{.*}} (%[[INDEX:[0-9]+]] = %[[J]],
// AFFINE: %[[ROW:[0-9]+]] = muli %[[I]], %[[N]] : !i32
// AFFINE: addi %[[ROW]], %[[J]] : !i32
// AFFINE: addi %[[ROW]], %[[K]] : !i32
// AFFINE: extsi %[[INDEX]] : !i64
// AFFINE: %[[NEXT:[0-9]+]] = addi %[[INDEX]], %[[N]] : !i32
// AFFINE-NEXT: -> %[[NEXT]],
