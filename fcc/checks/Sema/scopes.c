// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Name resolution: every use must see a declaration, and one scope admits one
// definition per name.

int undeclared(void) { return missing; }
// CHECK: [E0200] Error: use of undeclared identifier 'missing'
// CHECK: not declared in this scope
// CHECK: Help: declare 'missing' with a type before using it
// CHECK: N3220) 6.5.2p2

int redefined(void) { int value; int value; return 0; }
// CHECK: [E0201] Error: redefinition of 'value'
// CHECK: previous declaration is here
// CHECK: N3220) 6.7.1p4
// CHECK: n3220.pdf

int loop_scope(void) { for (int index = 0; index < 1; index++) ; return index; }
// CHECK: [E0200] Error: use of undeclared identifier 'index'

int block_scope(void) { { enum Local { Value = 5 }; } return Value; }
// CHECK: [E0200] Error: use of undeclared identifier 'Value'
