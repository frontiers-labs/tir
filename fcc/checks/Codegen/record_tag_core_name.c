// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

// A C tag that shares a core type name gets a name of its own.

// CHECK: type.def !tag_array.0 = !struct<{x: !i32}>
struct array { int x; };
int f(void) { return 0; }
