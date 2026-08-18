// RUN: fcc --explain E0300 | filecheck %s
// RUN: not fcc --explain nope 2>&1 | filecheck %s --check-prefix=UNKNOWN

// CHECK: error[E0300]: #error directive
// CHECK: active `#error` directive
// CHECK: Reference: C17 6.10.5

// UNKNOWN: fcc: error: unknown diagnostic code 'nope'
