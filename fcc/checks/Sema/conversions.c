// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Assignment-like contexts (assignment, return, initialization, casts,
// arguments) share the implicit conversion constraints.

int assign_integer_to_pointer(void) { int *pointer; pointer = 1; return 0; }
// CHECK: [E0404] Error: cannot assign value of integer type to pointer
// CHECK: N3220) 6.5.17.2p1

int *return_integer_as_pointer(void) { return 1; }
// CHECK: [E0404] Error: cannot return value of integer type as pointer
// CHECK: N3220) 6.8.7.5p3

int initialize_pointer_with_integer(void) { int *pointer = 1; return 0; }
// CHECK: [E0404] Error: cannot initialize pointer with integer value
// CHECK: N3220) 6.7.11p12

void sink(void);
int cast_void_to_integer(void) { return (int)sink(); }
// CHECK: [E0404] Error: cannot cast void expression to integer type
// CHECK: N3220) 6.5.5p2

int read(int *pointer);
int call_with_wrong_argument_type(void) { return read(1); }
// CHECK: [E0404] Error: argument 1 to 'read' has incompatible integer type
// CHECK: previous declaration is here
// CHECK: N3220) 6.5.3.3p2
