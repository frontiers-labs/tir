// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-restrict.tir %s
// RUN: tir opt --pass func.func(thread-state,instcombine,sccp,dce,dse) -o /tmp/fcc-matmul-restrict.base /tmp/fcc-matmul-restrict.tir
// RUN: tir opt --pass func.func(thread-state,instcombine,sccp,dce,dse,affine) -o /tmp/fcc-matmul-restrict.affine /tmp/fcc-matmul-restrict.tir
// RUN: cmp /tmp/fcc-matmul-restrict.base /tmp/fcc-matmul-restrict.affine

// The same nest over `restrict` pointers. The λ carries `noalias [0, 1, 2]`,
// but `thread-state` runs before the round that promotes the parameter slots,
// so at threading time every parameter is still read back from memory and the
// facts split nothing: the three objects share one chain, every pair across
// them is decided by a range predicate rather than a distance, and a predicate
// is versioning's input, not the scheduler's — it leaves the nest alone, byte
// for byte. Spec 055 records the pipeline order as plan-level.

void matmul_restrict_parameters(int *restrict a, int *restrict b,
                                int *restrict c)
{
    for (int i = 0; i < 64; i++)
        for (int j = 0; j < 64; j++)
            for (int k = 0; k < 64; k++)
                c[i * 64 + j] += a[i * 64 + k] * b[k * 64 + j];
}

// CHECK: matmul_restrict_parameters:
