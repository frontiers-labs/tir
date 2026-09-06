// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_do_while.c | filecheck %s

// A `do` is tail-controlled: the body runs before the condition, so the
// `scf.loop` body holds the increment and store ahead of the comparison, and
// the comparison alone selects the loop's predicate.

// CHECK: scf.loop (| %{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK: addi
// CHECK: ptr.store
// CHECK: %[[C:[0-9]+]] = cmpi {{.*}} {predicate = "slt"}
// CHECK: %[[P:[0-9]+]] = scf.switch %[[C]] {
// CHECK: -> %[[P]] |
