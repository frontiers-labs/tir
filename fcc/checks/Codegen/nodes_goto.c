// RUN: fcc compile --stage ir -o - %s | filecheck %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f sum_to --args=4 | filecheck --check-prefix=FOUR %s
// RUN: fcc compile --stage ir -o - %s | tir interp -f sum_to --args=4 | filecheck --check-prefix=FOUR %s

// A function holding a label is a flat graph of blocks; the backward `goto`
// becomes an `scf.loop` and the forward one an arm of the gamma inside it.

int sum_to(int limit) {
    int sum = 0;
    int value = 0;
again:
    if (value == limit)
        goto done;
    sum += value;
    value = value + 1;
    goto again;
done:
    return sum;
}

// CHECK: %{{[0-9]+}} = func.func @sum_to
// CHECK-NOT: cfg.br
// CHECK: scf.loop
// CHECK: scf.switch2
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
// CHECK-NEXT: }

// FOUR: i32 6
