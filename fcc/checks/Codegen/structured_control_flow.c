// RUN: fcc compile --stage ir -o - %s | filecheck %s

// The mid-end takes structured control flow only: no combination of C
// constructs may reach it as a branch.

int f(int n, int m) {
    int total = 0;
    int i;
    if (n < 0 && m > 0) {
        return -1;
    }
    for (i = 0; i < n; i = i + 1) {
        switch (i & 1) {
        case 0:
            total = total + (m ? i : -i);
            break;
        default:
            if (total > 100) {
                return total;
            }
            total = total - 2;
            break;
        }
        do {
            total = total - 1;
        } while (total > n);
    }
    while (total < 0 || n > m) {
        total = total + 1;
        if (total == 7) {
            return 7;
        }
    }
    return total;
}

// CHECK-NOT: cfg.
// CHECK: %{{[0-9]+}} = func.func @f(
// CHECK: scf.while
// CHECK-NOT: cfg.
