// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Pointer arithmetic requires complete, compatible pointee types.

int arithmetic_on_void_pointer(void *pointer) { return pointer + 1 != pointer; }
// CHECK: [E0402] Error: pointer arithmetic requires a pointer to a complete object type

long difference_of_incompatible_pointers(int *left, char *right) { return left - right; }
// CHECK: [E0402] Error: pointer subtraction requires pointers to compatible complete object types
