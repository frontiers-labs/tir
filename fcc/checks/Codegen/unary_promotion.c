// RUN: fcc compile --stage ir -o - %s | filecheck %s

// Unary arithmetic runs at the promoted type, so a narrow operand is widened
// before the operation rather than mixed into it.

typedef short ee_s16;

void take(ee_s16 value);

void negate(ee_s16 value)
{
  take(-value);
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @negate
// CHECK: %[[WIDE:[0-9]+]] = extsi %{{[0-9]+}} : !i32
// CHECK: subi %{{[0-9]+}}, %[[WIDE]] : !i32

void complement(ee_s16 value)
{
  take(~value);
}

// CHECK-LABEL: %{{[0-9]+}} = func.func @complement
// CHECK: %[[WIDE2:[0-9]+]] = extsi %{{[0-9]+}} : !i32
// CHECK: xori %[[WIDE2]], %{{[0-9]+}} : !i32
