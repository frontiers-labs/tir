// RUN: fcc compile --stage ir -o - %s | filecheck %s --check-prefix=IR
// RUN: fcc compile --march x86_64 --stage asm -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 --stage obj -o - %s | tir readobj - | filecheck %s --check-prefix=OBJ

// An object defined in another translation unit is declared here, so the
// address taken of it names a symbol the module knows.

extern int counter;

int bump(void)
{
  return counter + 1;
}

// IR: func.declare @counter
// IR: func.addr_of {sym_name = "counter"}

// CHECK-LABEL: bump:
// CHECK: counter

// The object stays undefined here: the linker resolves the reference.
// OBJ: Symbol counter: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
