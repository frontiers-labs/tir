// RUN: fcc compile --stage ir -o - %S/../Inputs/codegen_switch.c | filecheck %s

// CHECK: %{{[0-9]+}} = func.func @classify
// CHECK: cmpi {{.*}} {predicate = "eq"}
// CHECK: scf.if
// CHECK: cmpi {{.*}} {predicate = "eq"}
// CHECK: scf.if
// CHECK: cmpi {{.*}} {predicate = "eq"}
// CHECK: scf.switch
// CHECK: addi
// CHECK: ptr.store
// CHECK: func.return
