// RUN: fcc compile --stage ir --march x86_64 -o - %s | filecheck %s

// `p[i]` adds the value of `i` scaled by the element size, so an index
// narrower than a pointer is extended by its own signedness before
// `ptr.ptradd`, which takes a pointer-sized offset only.

int load_signed(int *p, int i) { return p[i]; }

// CHECK-LABEL: func.func @load_signed
// CHECK: %[[INDEX:[0-9]+]] = extsi %{{[0-9]+}} : !i64
// CHECK: %[[OFFSET:[0-9]+]] = muli %[[INDEX]], %{{[0-9]+}} : !i64
// CHECK: ptr.ptradd %{{[0-9]+}}, %[[OFFSET]] : !ptr.p

int load_unsigned(int *p, unsigned u) { return p[u]; }

// CHECK-LABEL: func.func @load_unsigned
// CHECK: %[[INDEX:[0-9]+]] = extui %{{[0-9]+}} : !i64
// CHECK: %[[OFFSET:[0-9]+]] = muli %[[INDEX]], %{{[0-9]+}} : !i64
// CHECK: ptr.ptradd %{{[0-9]+}}, %[[OFFSET]] : !ptr.p
