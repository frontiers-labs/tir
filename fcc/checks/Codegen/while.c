// RUN: fcc compile --stage ir -o - %S/../Inputs/basic_while.c | filecheck %s

// The original condition controls repetition. The switch carries the state
// from the exit arm or the body that performs the store.

// CHECK: scf.loop (%{{[0-9]+}} = %{{[0-9]+}}) {
// CHECK: %[[C:[0-9]+]] = cmpi {{.*}} {predicate = "slt"}
// CHECK: %[[S:[0-9]+]] = scf.switch %[[C]] args(%{{[0-9]+}}) (%[[EXIT:[0-9]+]]) {
// CHECK: -> %[[EXIT]]
// CHECK: addi
// CHECK: %[[STORED:[0-9]+]] = ptr.store
// CHECK: -> %[[STORED]]
// CHECK: -> %[[C]], %[[S]], %[[S]]
