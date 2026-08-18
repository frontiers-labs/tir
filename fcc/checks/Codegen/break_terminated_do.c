// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A `do` body ending in `break` leaves before the condition is evaluated, so
// the condition must not be inlined into the loop that remains.

int bump(void);
int f(int n) {
    int total = 0;
    do {
        total = total + n;
        break;
    } while (bump());
    return total;
}

// CHECK-NOT: cfg.
// CHECK: func.func @f(
// CHECK-NOT: func.call @bump
