// RUN: not fcc compile --std c23 --stage ir -o - %s 2>&1 | filecheck %s

// Call and return constraints.

int add(int left, int right);
int wrong_argument_count(void) { return add(1); }
// CHECK: [E0406] Error: function 'add' expects 2 arguments but 1 was provided
// CHECK: previous declaration is here
// CHECK: N3220) 6.5.3.3p2

int not_a_function(void) { int value; return value(); }
// CHECK: [E0405] Error: called object 'value' is not a function
// CHECK: previous declaration is here
// CHECK: N3220) 6.5.3.3p1

void stop(void) { return 1; }
// CHECK: [E0505] Error: void function must not return a value
// CHECK: N3220) 6.8.7.5p1
