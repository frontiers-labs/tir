// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Declaration constraints: specifier combinations, qualifiers, object types,
// literal suffixes and declaration compatibility.

int invalid_specifiers(void) { unsigned float value; return 0; }
// CHECK: [E0400] Error: invalid type specifier combination 'unsigned float'
// CHECK: N3220) 6.7.3.1p2

int restrict_non_pointer(void) { restrict int value; return value; }
// CHECK: [E0408] Error: restrict qualifier requires a pointer-derived object type
// CHECK: N3220) 6.7.4.1p3

int void_object(void) { void value; return 0; }
// CHECK: [E0409] Error: object 'value' cannot have void type
// CHECK: N3220) 6.2.5p24

int sizeof_void(void) { return sizeof(void); }
// CHECK: [E0409] Error: sizeof requires a complete object type
// CHECK: N3220) 6.5.4.4p2

int invalid_suffix(void) { return 1ulul; }
// CHECK: [E0401] Error: invalid integer suffix in '1ulul'
// CHECK: N3220) 6.4.4.1p1

int convert(int value);
long convert(int value);
// CHECK: [E0202] Error: conflicting declarations for 'convert'
// CHECK: previous declaration is here
// CHECK: N3220) 6.7.1p5
