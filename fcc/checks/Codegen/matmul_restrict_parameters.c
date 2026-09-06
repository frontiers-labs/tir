// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-restrict.tir %s
// RUN: tir opt --pass func.func(promote-nodes,verify-deps,instcombine-nodes,affine) /tmp/fcc-matmul-restrict.tir | filecheck %s --check-prefix=IR

// The same nest over `restrict` pointers. Construction promotes the parameter
// slots before the chains are drawn, so the λ's `noalias [0, 1, 2]` reaches the
// facts as three objects rather than three reads of memory: each gets a chain of
// its own, every pair across them is a distance the scheduler can read, and the
// nest is reordered exactly as the local-array kernel's is — `k` out of the
// innermost position so the read of `b` walks a row. Spec 055 recorded the
// pipeline order as plan-level; this is the order it asked for.

void matmul_restrict_parameters(int *restrict a, int *restrict b,
                                int *restrict c)
{
    for (int i = 0; i < 64; i++)
        for (int j = 0; j < 64; j++)
            for (int k = 0; k < 64; k++)
                c[i * 64 + j] += a[i * 64 + k] * b[k * 64 + j];
}

// IR-LABEL: func.func @matmul_restrict_parameters
// IR: scf.for2 {{.*}} (%[[I:[0-9]+]] = {{.*}}) {
// IR-NEXT: scf.for2 {{.*}} (%[[K:[0-9]+]] = {{.*}}) {
// IR-NEXT: scf.for2 {{.*}} (%[[J:[0-9]+]] = {{.*}}) {
// IR: shli %[[I]]
// IR: addi %{{[0-9]+}}, %[[J]]
// IR: shli %[[I]]
// IR: addi %{{[0-9]+}}, %[[K]]

// CHECK: matmul_restrict_parameters:
