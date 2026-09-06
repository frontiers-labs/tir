// RUN: fcc compile --stage ir -o - %s | filecheck %s

void xor_u16(unsigned short *value)
{
    *value ^= 0x4002;
}

// The usual arithmetic conversions promote the narrow lvalue to int, the
// operation runs at int width, and the result is truncated back for the store.
// CHECK: %{{[0-9]+}} = func.func @xor_u16
// CHECK: %[[LOAD:[0-9]+]] | %{{[0-9]+}} = ptr.load %{{[0-9]+}} | %{{[0-9]+}} : !i16
// CHECK: %[[WIDE:[0-9]+]] = extui %[[LOAD]] : !i32
// CHECK: %[[XOR:[0-9]+]] = xori %[[WIDE]], %{{[0-9]+}} : !i32
// CHECK: %[[NARROW:[0-9]+]] = trunci %[[XOR]] : !i16
// CHECK: ptr.store %[[NARROW]], %{{[0-9]+}} | %{{[0-9]+}}
