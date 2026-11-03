// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-restrict.tir %s
// RUN: tir opt --pass func.func(promote-nodes,verify-deps,instcombine-nodes,affine) /tmp/fcc-matmul-restrict.tir | filecheck %s --check-prefix=IR

// The same nest over `restrict` pointers. The frontend spills each parameter
// into a slot it never assigns again, so the object a subscript is derived from
// is read back through that one spill: the λ's `noalias [0, 1, 2]` then makes
// the three parameters three objects, and `restructure-nodes` gives each a
// chain of its own. No pair crosses two chains, every pair within one is a
// distance the scheduler can read, and the nest is reordered exactly as the
// local-array kernel's — `k` out of the innermost position so the read of `b`
// walks a row rather than a column.

void matmul_restrict_parameters(int *restrict a, int *restrict b,
                                int *restrict c)
{
    for (int i = 0; i < 64; i++)
        for (int j = 0; j < 64; j++)
            for (int k = 0; k < 64; k++)
                c[i * 64 + j] += a[i * 64 + k] * b[k * 64 + j];
}

// IR-LABEL: func.func @matmul_restrict_parameters
// IR: scf.for %[[I:[0-9]+]] = %{{[0-9]+}} to %{{[0-9]+}} step %{{[0-9]+}}
// IR-NEXT: scf.for %[[K:[0-9]+]] = %{{[0-9]+}} to %{{[0-9]+}} step %{{[0-9]+}}
// IR-NEXT: scf.for %[[J:[0-9]+]] = %{{[0-9]+}} to %{{[0-9]+}} step %{{[0-9]+}}
// IR: %[[ROW:[0-9]+]] = shli %[[I]]
// IR-NEXT: addi %[[ROW]], %[[J]]
// IR: addi %[[ROW]], %[[K]]
// IR: %[[BROW:[0-9]+]] = shli %[[K]]
// IR-NEXT: addi %[[BROW]], %[[J]]

// CHECK: matmul_restrict_parameters:
