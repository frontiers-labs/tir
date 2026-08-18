// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// An initializer list may not provide more initializers than the aggregate has
// elements, counting positions a designator has advanced past.

int excess_array(void) { int values[1] = {11, 22}; return 0; }
// CHECK: [E0402] Error: too many initializers for array
// CHECK: N3220) 6.7.11p12

struct Pair { int value; };
int excess_record(void) { struct Pair pair = {11, 22}; return 0; }
// CHECK: [E0402] Error: too many initializers for record

struct Two { int left; int right; };
int record_after_designator(void) { struct Two pair = {.right = 22, 11}; return 0; }
// CHECK: [E0402] Error: too many initializers for record

int array_after_designator(void) { int values[2] = {[1] = 12, 30}; return 0; }
// CHECK: [E0402] Error: too many initializers for array

union Value { int integer; long wide; };
int excess_union(void) { union Value value = {11, 22}; return 0; }
// CHECK: [E0402] Error: too many initializers for union
