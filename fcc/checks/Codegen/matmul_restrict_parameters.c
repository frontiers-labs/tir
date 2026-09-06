// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o /tmp/fcc-matmul-restrict.tir %s
// RUN: tir opt --pass func.func(promote-nodes,verify-deps,instcombine-nodes,affine) /tmp/fcc-matmul-restrict.tir | filecheck %s --check-prefix=IR

// The same nest over `restrict` pointers. The λ's `noalias [0, 1, 2]` makes
// the three parameters three objects, and with per-object chains the nest was
// reordered exactly as the local-array kernel's: `k` out of the innermost
// position so the read of `b` walks a row. Today the order stays `i, j, k` for
// the same reason as there: `j` is stored to its slot in the `j` loop and
// reloaded inside the `k` loop, promote-nodes does not forward the port across
// the loop boundary, and the `c` and `b` subscripts are sums over a `ptr.load`
// rather than over the counter. The affine view refuses the interchange. This
// pins the reload and the unmoved order.

void matmul_restrict_parameters(int *restrict a, int *restrict b,
                                int *restrict c)
{
    for (int i = 0; i < 64; i++)
        for (int j = 0; j < 64; j++)
            for (int k = 0; k < 64; k++)
                c[i * 64 + j] += a[i * 64 + k] * b[k * 64 + j];
}

// IR-LABEL: func.func @matmul_restrict_parameters
// IR: scf.for {{.*}} (%[[I:[0-9]+]] = {{.*}}) {
// IR: scf.for {{.*}} (%[[J:[0-9]+]] = {{.*}}) {
// IR: ptr.store %[[J]], %[[JSLOT:[0-9]+]] |
// IR: scf.for {{.*}} (%[[K:[0-9]+]] = {{.*}}) {
// IR: %[[JL:[0-9]+]] | %{{[0-9]+}} = ptr.load %[[JSLOT]] |
// IR: %[[ROW:[0-9]+]] = shli %[[I]]
// IR-NEXT: addi %[[ROW]], %[[JL]]
// IR: addi %[[ROW]], %[[K]]
// IR: shli %[[K]]

// CHECK: matmul_restrict_parameters:
