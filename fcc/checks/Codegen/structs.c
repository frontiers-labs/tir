// RUN: fcc compile --stage ir -o - %S/../Inputs/structs.c | filecheck %s

// Struct access reaches the IR as pointer arithmetic: the frontend's layout
// operations are gone before the mid-end sees a function body. The copy's
// field loads and stores form one dependency chain, so the copy keeps its
// order without a block to sit in.

// CHECK-NOT: cir.
// CHECK: %{{[0-9]+}} = func.func @read(%{{[0-9]+}}: !ptr.p) -> !i32 {
// CHECK: %[[OFF:[0-9]+]] = constant {value = 4} : !i64
// CHECK: ptr.ptradd %{{[0-9]+}}, %[[OFF]] : !ptr.p
// CHECK: %{{[0-9]+}} = func.func @copy() -> !i32 {
// CHECK: ptr.alloca {size = 8, align = 4} : !ptr.p
// CHECK: %[[TAG:[0-9]+]] | %[[TAG_LOAD:[0-9]+]] = ptr.load %{{[0-9]+}} | %{{[0-9]+}} : !i8
// CHECK: | %[[TAG_STORE:[0-9]+]] = ptr.store %[[TAG]], %{{[0-9]+}} | %[[TAG_LOAD]]
// CHECK: %[[VALUE:[0-9]+]] | %[[VALUE_LOAD:[0-9]+]] = ptr.load %{{[0-9]+}} | %[[TAG_STORE]] : !i32
// CHECK: ptr.store %[[VALUE]], %{{[0-9]+}} | %[[VALUE_LOAD]]
