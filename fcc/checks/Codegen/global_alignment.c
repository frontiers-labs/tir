// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s
// RUN: fcc compile --stage obj --march x86_64 -o - %s | tir readobj - | filecheck %s --check-prefix=OBJ

// Global objects respect their natural alignment: the assembly aligns each
// symbol, and in the object the symbol lands on an aligned offset in an
// aligned .data.

char prefix = 1;
long value = 42;

// CHECK: prefix:
// CHECK: .balign 8
// CHECK-NEXT: value:

// OBJ: Section .data: type=PROGBITS flags=WA size=0x10 align=8
// OBJ: Symbol prefix: value=0x0 size=0x1
// OBJ: Symbol value: value=0x8 size=0x8
