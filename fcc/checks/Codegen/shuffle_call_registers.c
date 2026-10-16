// REQUIRES: linux, x86_64
// RUN: env TIR_SHUFFLE_SEED=0 fcc compile --march x86_64 -O2 --stage obj --shuffle-machine-order -o - %s | python3 %S/../Inputs/run_object.py
// RUN: env TIR_SHUFFLE_SEED=1 fcc compile --march x86_64 -O2 --stage obj --shuffle-machine-order -o - %s | python3 %S/../Inputs/run_object.py

// Shuffling may schedule the loop's own arithmetic between the code that
// computes a call's arguments, `al` for the variadic call among them, and the
// call. Nothing scheduled there may take an argument's register.

#include <stdio.h>
#include <string.h>

__attribute__((noinline)) static long mix(long a, long b, long c, long d) {
    return ((a - b) * c) ^ d;
}

int main(void) {
    int arr[8] = {-400, 833, 246, 822, -951, -509, -173, 661};
    char line[64];
    long total = 0;
    for (int i = 0; i < 8; i++) {
        long r = mix(arr[i], i * 3 + 1, arr[(i + 2) & 7], i);
        snprintf(line, sizeof line, "%d %ld %f", arr[(i + 5) & 7], r, i * 0.5);
        total += (long)strlen(line) + r;
    }
    return total == 371179 ? 0 : (int)(total & 0xff) | 1;
}
