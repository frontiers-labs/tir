// RUN: fcc compile --stage ir --nodes -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f stop_early --args=6 | filecheck --check-prefix=SIX %s
// RUN: fcc compile --stage ir --nodes -o - %s | tir interp -f stop_early --args=6 | filecheck --check-prefix=SIX %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f stop_early --args=2 | filecheck --check-prefix=TWO %s
// RUN: fcc compile --stage ir --nodes -o - %s | tir interp -f stop_early --args=2 | filecheck --check-prefix=TWO %s

// `continue` and `break` make the body leave two ways, so the loop expands to
// blocks and the conversion makes it an `scf.loop`: each way out is a gamma
// arm, and the exit an iteration took is carried out and dispatched on.

int stop_early(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        if (i == 1) continue;
        if (i == 3) break;
        total = total + i;
    }
    return total + i;
}

// CHECK: %{{[0-9]+}} = func.func @stop_early
// CHECK-NOT: scf.while
// CHECK: scf.loop
// CHECK: scf.switch2
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
// CHECK-NEXT: }

// SIX: i32 5
// TWO: i32 2
