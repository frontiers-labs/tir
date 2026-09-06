// RUN: fcc compile --stage ir -o - %s | filecheck %s

// Three shapes the counted-loop recognition refuses, each falling back to the
// blocks and branches the frontend used to emit and so to `restructure-nodes`'s
// `scf.loop`, whose body re-reads the counter and tests it itself. A refusal is
// a missed optimisation, never a miscompilation.

void sink(int *p);

// The body writes the counter, so the loop does not count.
int mutated_counter(void) {
    int i;
    int total = 0;
    for (i = 0; i < 3; i = i + 1) {
        i = i + 5;
        total = total + 1;
    }
    return total;
}

// The counter's address is taken, so what the loop counts through is not private
// to the loop.
int escaping_counter(void) {
    int i;
    int total = 0;
    sink(&i);
    for (i = 0; i < 3; i = i + 1) {
        total = total + 1;
    }
    return total;
}

// The bound is written through a pointer derived from its slot, so reading it
// once before the loop would not be reading it at all.
int mutated_bound(void) {
    int limit = 10;
    int i;
    int total = 0;
    for (i = 0; i < limit; i = i + 1) {
        total = total + 1;
        (&limit)[0] = 3;
    }
    return total;
}

// The step is not a constant, so the trip count is not the bounds' business.
int variable_step(int s) {
    int i;
    int total = 0;
    for (i = 0; i < 10; i = i + s) {
        total = total + 1;
    }
    return total;
}

// CHECK: func.func @mutated_counter
// CHECK-NOT: scf.for2
// CHECK: scf.loop
// CHECK: func.func @escaping_counter
// CHECK-NOT: scf.for2
// CHECK: scf.loop
// CHECK: func.func @mutated_bound
// CHECK-NOT: scf.for2
// CHECK: scf.loop
// CHECK: func.func @variable_step
// CHECK-NOT: scf.for2
// CHECK: scf.loop
