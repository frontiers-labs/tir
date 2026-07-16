#define ADD(left, right) ((left) + (right))
#define APPLY(function, left, right) function(left, right)
int result = APPLY(ADD, 2, 3);
