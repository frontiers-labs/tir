// RUN: fcc compile --stage ir -o - %S/../Inputs/early_return.c | filecheck %s

// An early return leaves the loop carrying the exit it took; the `scf.switch`
// after the loop picks the value that exit returns, and the function keeps a
// single `func.return`.

// CHECK: func.func @find
// CHECK: scf.while iter_args
// CHECK: scf.condition
// CHECK: scf.switch %{{[0-9]+}} case 0 {
// CHECK: constant {value = -1}
// CHECK: default {
// CHECK: func.return
