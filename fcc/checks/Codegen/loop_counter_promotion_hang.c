// RUN: fcc compile --stage obj --march x86_64 -o /tmp/fcc-lcp-hang.o %s
// RUN: cc /tmp/fcc-lcp-hang.o -o /tmp/fcc-lcp-hang.bin
// RUN: timeout 5 /tmp/fcc-lcp-hang.bin

// Found by `cargo xtask fcc-fuzz` (reduced from seed 127): instcombine wires a
// foreign constant into a nested rotated loop's counter yield, so the counter
// never advances and the loop never exits. Counted loops no longer reach that
// shape — they are raised to `scf.for` instead of restructured into a rotated
// `scf.while` — so this program now terminates and must keep doing so. The
// instcombine defect itself is fixed too: `loop_counter_promotion_hang_flat.c`
// is the same reduction forced down the flat path, and terminates.

int f0(int a, int b) {
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
