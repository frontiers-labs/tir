// RUN: fcc compile --stage ir -o - %S/../Inputs/structs.c | filecheck %s

// Struct access reaches the IR as pointer arithmetic: the frontend's layout
// operations are gone before the mid-end sees a function body. The copy reads
// both fields off the source slot's chain and writes them in order on the
// destination's, so it keeps its order without a block to sit in.

// CHECK-NOT: cir.
// CHECK: %{{[0-9]+}} = func.func @read(%{{[0-9]+}}: !ptr.p) -> !i32 {
// CHECK: %[[OFF:[0-9]+]] = constant {value = 4} : !i64
// CHECK: ptr.ptradd %{{[0-9]+}}, %[[OFF]] : !ptr.p
// CHECK: %{{[0-9]+}} = func.func @copy() -> !i32 {
// CHECK: ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK: %[[TAG:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %{{[0-9]+}} state(%[[SRC:[0-9]+]]) : !i8
// CHECK: state(%[[TAG_STORE:[0-9]+]]) = ptr.store %[[TAG]], %{{[0-9]+}} state(%{{[0-9]+}})
// CHECK: %[[VALUE:[0-9]+]], state(%{{[0-9]+}}) = ptr.load %{{[0-9]+}} state(%[[SRC]]) : !i32
// CHECK: ptr.store %[[VALUE]], %{{[0-9]+}} state(%[[TAG_STORE]])
