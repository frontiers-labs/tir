// RUN: fcc cc -E %s 2>&1 | filecheck %s

// An #error inside a skipped conditional group is not a diagnostic. The
// patterns are spelled as regexes so the comments themselves do not match.

#if 0
#error never
#endif
int alive;

// CHECK-NOT: {{\[E0300\]}}
// CHECK: int alive;
// CHECK-NOT: {{\[E0300\]}}
