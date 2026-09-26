// RUN: fcc compile --stage obj --march x86_64 -o /tmp/fcc-comma-precedence-lit.o %s
// RUN: cc /tmp/fcc-comma-precedence-lit.o -o /tmp/fcc-comma-precedence-lit
// RUN: /tmp/fcc-comma-precedence-lit

unsigned long long collapse_state(unsigned long long state, int pos) {
    int k;
    unsigned long long mask = 0;
    for (k = sizeof(unsigned long long) * 8 - 1, mask = 0; k > pos; k--)
        mask += (unsigned long long)1 << k;
    return (mask & state) >> 1;
}

int main(void) {
    int left = 0, right = 0;
    left = 3, right = 4;
    return collapse_state(262144ULL, 0) != 131072ULL || left != 3 || right != 4;
}
