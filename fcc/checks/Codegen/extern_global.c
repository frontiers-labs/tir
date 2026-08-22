// RUN: fcc compile --stage ir -o - %s | filecheck %s --check-prefix=IR
// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --stage obj -o - %s | tir readobj - | filecheck %s --check-prefix=OBJ

// An object defined in another translation unit is declared by a δ with no
// storage, and the reference to it uses the value that declaration produces.

extern int counter;

int bump(void)
{
  return counter + 1;
}

// IR: %[[COUNTER:[0-9]+]] = global external @counter
// IR: ptr.load %[[COUNTER]]

// CHECK-LABEL: bump:
// CHECK: counter

// The object stays undefined here: the linker resolves the reference.
// OBJ: Symbol counter: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
