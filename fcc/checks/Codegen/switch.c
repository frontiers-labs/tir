// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_switch.c | filecheck %s

// CHECK: func.func @classify
// CHECK: cmpi {{.*}} {predicate = "eq"}
// CHECK: cmpi {{.*}} {predicate = "eq"}
// CHECK: cmpi {{.*}} {predicate = "eq"}
// CHECK: cir.if
// CHECK: ptr.store
// CHECK: cir.if
// CHECK: addi
// CHECK: ptr.store
// CHECK: func.return
