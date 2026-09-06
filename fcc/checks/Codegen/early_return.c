// RUN: fcc compile --stage ir -o - %S/../Inputs/early_return.c | filecheck %s

// An early return leaves the loop carrying the exit it took; the `scf.switch2`
// after the loop selects on that value which result the exit returns, and the
// function keeps a single return.

// CHECK: %{{[0-9]+}} = func.func @find
// CHECK: %[[EXIT:[0-9]+]] | %[[S:[0-9]+]] = scf.loop (%{{[0-9]+}} = %{{[0-9]+}} | %{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK: scf.switch2 %[[EXIT]] args(| %[[S]]) (| %{{[0-9]+}}) {
// CHECK-NEXT: constant {value = -1}
// CHECK: }
// CHECK-NEXT: (| %{{[0-9]+}}) {
// CHECK: ptr.load
// CHECK: -> %{{[0-9]+}} | %{{[0-9]+}}
// CHECK-NEXT: }
// CHECK-NEXT: module_end
