// RUN: fcc compile --stage obj --march x86_64 -o /tmp/fcc-lcp-hang-flat.o %s
// RUN: cc /tmp/fcc-lcp-hang-flat.o -o /tmp/fcc-lcp-hang-flat.bin
// RUN: timeout 5 /tmp/fcc-lcp-hang-flat.bin

// The reduction of `loop_counter_promotion_hang.c` with a label in it, which
// lowers every loop in the function flat and so restructures them into the
// rotated `scf.while` the instcombine defect needed. The label is the whole
// difference: keeping this reproducer is what stopped loop raising from hiding a
// bug it did not fix. What fixed it is law S2's proviso being stated in both
// directions — the state the overwritten store was handed may be read by that
// store and nothing else — which is what stops a store landing in a class other
// operations already answer from.

int f0(int a, int b) {
    goto start;
start:;
    for (int i = 0; i < 6; i++) {
        for (int i = 0; i < 1; i++) {
            for (int i = 0; i < 5; i++) {
                b = i;
                int v1 = i;
                b = v1;
            }
            for (int i = 0; i < 2; i++) {
                int v2 = b;
                for (int i = 0; i < 6; i++) {
                    b = v2;
                    for (int i = 0; i < 4; i++) {
                        a = i;
                    }
                    b = a;
                }
                for (int i = 0; i < 4; i++) {
                    a = i;
                    for (int i = 0; i < 4; i++) {
                        int v3 = -44;
                    }
                    for (int i = 0; i < 6; i++) {
                        a = 11;
                        for (int i = 0; i < 1; i++) {
                            for (int i = 0; i < 1; i++) {
                                b = i;
                                int v4 = b;
                            }
                        }
                        for (int i = 0; i < 1; i++) {
                            int v5 = i;
                            int v6 = 62;
                        }
                    }
                }
            }
        }
        a = (46 >> 3);
        a = b;
    }
    if (((-(22) / (((a & 7) + 1))) == -45)) {
        b = (45 + -60);
        b = a;
    }
    return 22;
}
int main(void) {
    return f0(1, 2) - 22;
}
