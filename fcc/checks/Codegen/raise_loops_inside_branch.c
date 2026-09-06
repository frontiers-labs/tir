// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A raised loop is an ordinary non-terminator operation sitting in a block, so
// the blocks around it still restructure: the `if` here reaches
// `restructure-nodes` as a CFG diamond and comes back as a `scf.switch2` whose
// taken arm carries the `scf.for2` along. Nothing branches in the result.

int pick(int flag, int n) {
    int i;
    int total = 0;
    if (flag) {
        for (i = 0; i < n; i = i + 1) {
            total = total + i;
        }
    } else {
        total = 1;
    }
    return total;
}

// CHECK: func.func @pick
// CHECK: scf.switch2
// CHECK: }
// CHECK: scf.for2 %{{[0-9]+}} = %{{[0-9]+}} to %{{[0-9]+}} step %{{[0-9]+}} (
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
// CHECK: }
// CHECK-NOT: cfg.
// CHECK-NOT: scf.loop
