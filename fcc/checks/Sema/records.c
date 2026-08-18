// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Record constraints: member access, member uniqueness, completeness.

struct Pair { int value; };
int unknown_member(void) { struct Pair pair; return pair.missing; }
// CHECK: [E0402] Error: struct 'Pair' has no member named 'missing'
// CHECK: N3220) 6.5.3.2p2

struct Dup { int value; char value; };
// CHECK: [E0201] Error: redefinition of 'value'
// CHECK: this declaration redefines 'value'
// CHECK: previous declaration is here

struct Opaque;
int incomplete_object(void) { struct Opaque object; return 0; }
// CHECK: [E0409] Error: object 'object' has incomplete struct type

int incomplete_array(void) { int values[]; return 0; }
// CHECK: [E0409] Error: object 'values' has incomplete array type
