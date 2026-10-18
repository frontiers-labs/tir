// REQUIRES: linux, x86_64
// RUN: env TIR_SHUFFLE_SEED=2 fcc compile --march x86_64 -O2 --stage obj --shuffle-machine-order -o - %s | python3 %S/../Inputs/run_object.py
// RUN: env TIR_SHUFFLE_SEED=3 fcc compile --march x86_64 -O2 --stage obj --shuffle-machine-order -o - %s | python3 %S/../Inputs/run_object.py

// `cdq` writes edx and `idiv` reads it. These seeds schedule the loop's other
// arithmetic between the two; a value defined there must not take edx, or the
// division traps.

#include <stdio.h>

int main(void) {
    int mat[8][8];
    for (int i = 0; i < 8; i++)
        for (int j = 0; j < 8; j++)
            mat[i][j] = (i % 3) & 255;
    for (int i = 0; i < 8; i++)
        for (int j = 0; j < 8; j++)
            mat[j][i] = (mat[j][i] + (j / ((i & 7) + 1))) & 255;
    int sum = 0;
    for (int i = 1; i < 8; i++)
        for (int j = 0; j < 7; j++) {
            printf("%d\n", mat[i][j]);
            sum += mat[i][j];
        }
    return sum == 106 ? 0 : 1;
}
