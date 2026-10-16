// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s

// A tag the core spells a type with gets a name of its own, so the C array
// below still lays out as the core's `!array`.

// CHECK: type.def !tag_array.0 = !struct<{x: !i32}>
// CHECK: constant {value = 12} : !i64
struct array { int x; };
int f(void) { int a[3]; return sizeof a; }
