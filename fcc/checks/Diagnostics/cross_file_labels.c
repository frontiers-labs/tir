// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// A related label may point into another file: the report shows both frames,
// each with its own source text.

#include "Inputs/decl.h"
long value;
// CHECK: [E0202] Error: conflicting declarations for 'value'
// CHECK: cross_file_labels.c:7:1 ]
// CHECK: long value;
// CHECK: this declaration has an incompatible type
// CHECK: decl.h:1:1 ]
// CHECK: int value;
// CHECK: previous declaration is here
