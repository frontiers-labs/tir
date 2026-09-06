// RUN: fcc compile --stage ir -o - %s | filecheck %s

// `break` and `continue` in a `for`, and `continue` in a `do`, are loop exits
// whose C meaning does not match the `scf.loop` body they land in: the step
// and the trailing condition must still run on a `continue`, structurally.

int f(int n) {
    int total = 0;
    int i;
    int j;
    for (i = 0; i < n; i = i + 1) {
        if (i == 1) {
            continue;
        }
        if (i == 7) {
            break;
        }
        total = total + i;
    }
    j = 0;
    do {
        j = j + 1;
        if (j == 2) {
            continue;
        }
        total = total + j;
    } while (j < n);
    for (i = 0; i < n; i = i + 1) {
        while (total > 0) {
            total = total - 1;
            if (total == 3) {
                continue;
            }
            if (total == 1) {
                break;
            }
        }
        if (i == 2) {
            continue;
        }
        total = total + 1;
    }
    return total;
}

// CHECK-NOT: cfg.
// CHECK: %{{[0-9]+}} = func.func @f(
// CHECK: scf.loop
// CHECK-NOT: cfg.
