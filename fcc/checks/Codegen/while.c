// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_while.c | filecheck %s

// A `while` is an `scf.loop` whose body tests the condition first: the
// `scf.switch` on it yields a false predicate in arm 0 (exit) and runs the
// body and yields true in arm 1 (continue), and the loop's predicate is the
// arm's result.

// CHECK: scf.loop (| %{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK: %[[C:[0-9]+]] = cmpi {{.*}} {predicate = "slt"}
// CHECK: %[[P:[0-9]+]] | %[[S:[0-9]+]] = scf.switch %[[C]] args(
// CHECK: %[[F:[0-9]+]] = constant {value = 0} : !i1
// CHECK-NEXT: -> %[[F]]
// CHECK: addi
// CHECK: ptr.store
// CHECK: %[[T:[0-9]+]] = constant {value = 1} : !i1
// CHECK-NEXT: -> %[[T]]
// CHECK: -> %[[P]] | %[[S]], %[[S]]
