// RUN: fcc compile --stage ir -o - %s | filecheck %s

// A pointer converted to an integer (and back) is address arithmetic against
// the null pointer, so the value carries the right type at every step.

typedef unsigned long uptr;

uptr address(void *pointer)
{
  return (uptr)pointer - 1;
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @address
// CHECK: %[[NULL:[0-9]+]] = ptr.null : !ptr.p
// CHECK: ptr.ptrdiff %{{[0-9]+}}, %[[NULL]] : !i64

void *aligned(void *pointer)
{
  return (void *)(((uptr)pointer + 3) & ~3);
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @aligned
// CHECK: ptr.null : !ptr.p
// CHECK: %[[NULL2:[0-9]+]] = ptr.null : !ptr.p
// CHECK: ptr.ptrdiff
// CHECK: ptr.ptradd %[[NULL2]], %{{[0-9]+}} : !ptr.p
