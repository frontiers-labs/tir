// RUN: fcc compile -O2 --stage obj --march x86_64 -o %t.o %s
// RUN: fcc compile -O2 --stage obj --march x86_64 --shuffle-machine-order -o %t.o %s

// Nightly differential-fuzz #448 #449 #450. A nest writes `A[j][i] + (i + 1)`.
// Instcombine CSEs `i + 1` with the outer latch. Affine interchanges the nest
// for the column walk, then cloned the innermost body without remapping that
// latch, so the old increment died and fcc panicked on `live value`.

#include <stdio.h>

int main(void)
{
    int mat[8][8];
    for (int i = 0; i < 8; i++)
        for (int j = 0; j < 8; j++)
            mat[j][i] = (mat[j][i] + (i + 1)) & 255;
    for (int i = 1; i < 8; i++)
        for (int j = 0; j < 7; j++)
            printf("%d\n", mat[i][j]);
    return 0;
}
