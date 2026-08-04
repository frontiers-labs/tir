// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s

int pointer_condition(void *pointer)
{
  return pointer ? 1 : 0;
}

// CHECK-LABEL: pointer_condition:
// CHECK: cmp r{{[a-z0-9]+}}, 0
