// RUN: fcc compile -O2 --stage ir -o - %s | filecheck %s
// RUN: fcc compile -O2 --stage obj --march x86_64 -o /tmp/fcc-transposed-matrix.o %s
// RUN: fcc compile -O2 --stage obj --march x86_64 --pipeline func.func(promote-nodes),fixpoint<3>(func.func(verify-deps,instcombine-nodes,affine,instcombine-nodes)) -o /tmp/fcc-transposed-matrix-affine.o %s
// RUN: fcc compile -O2 --stage obj --march x86_64 --shuffle-machine-order -o /tmp/fcc-transposed-matrix-shuffle.o %s

// Nightly differential-fuzz #448 #449 #450. An 8x8 nest writes `m[j][i] += i + 1`.
// instcombine folds that add with the outer latch; affine interchange then
// rebuilt the nest while the cloned body still named the erased latch, and fcc
// panicked (`live value`) on the default -O2 pipeline, the affine pipeline, and
// machine-order shuffling.

#include <stdio.h>
int main(void) {
    int m[8][8];
    for (int i = 0; i < 8; i++)
        for (int j = 0; j < 8; j++)
            m[j][i] = (m[j][i] + ((i + 1))) & 255;
    for (int i = 1; i < 8; i++)
        for (int j = 0; j < 8 - 1; j++)
            printf("%d\n", m[i][j]);
    return 0;
}

// CHECK: func.func @main
// CHECK: scf.for
