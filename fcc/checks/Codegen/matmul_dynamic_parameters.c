// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-dynamic.tir %s
// RUN: tir opt --pass func.func(promote,thread-state,instcombine) -o /tmp/fcc-matmul-dynamic.base /tmp/fcc-matmul-dynamic.tir
// RUN: tir opt --pass func.func(promote,thread-state,instcombine,affine) -o /tmp/fcc-matmul-dynamic.affine /tmp/fcc-matmul-dynamic.tir
// RUN: cmp /tmp/fcc-matmul-dynamic.base /tmp/fcc-matmul-dynamic.affine

// The shape C code actually has: plain pointers and a row length nothing knows.
// `i * n` is a product of a counter and a parameter, which is no affine form at
// all, so every access here is refused before aliasing is even asked about.

void matmul_dynamic_parameters(int *a, int *b, int *c, int n)
{
    for (int i = 0; i < n; i++)
        for (int j = 0; j < n; j++)
            for (int k = 0; k < n; k++)
                c[i * n + j] += a[i * n + k] * b[k * n + j];
}

// CHECK: matmul_dynamic_parameters:
