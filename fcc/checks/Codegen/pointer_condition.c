// IR migration: instruction selection and backend lowering are unavailable.
// XFAIL: *
// RUN: fcc compile --stage ir -o - %s | filecheck %s --check-prefix=IR
// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s

int pointer_condition(void *pointer)
{
  return pointer ? 1 : 0;
}

// A pointer tested for truth compares against the null pointer, not an integer
// zero: the comparison is ptr.cmp, and no cmpi ever takes a pointer operand.
// IR: ptr.cmp %{{[0-9]+}}, %{{[0-9]+}} {predicate = "ne"} : !i1
// IR-NOT: cmpi

// CHECK-LABEL: pointer_condition:
// CHECK: cmp r{{[a-z0-9]+}}, 0
