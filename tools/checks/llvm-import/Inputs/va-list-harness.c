#include <assert.h>

extern int first_int(int, ...);
extern double first_double(int, ...);
extern int overflow_int(int, int, int, int, int, int, ...);
extern double overflow_double(int, double, double, double, double, double, double,
                              double, double, ...);

int main(void) {
    assert(first_int(0, 37) == 37);
    assert(first_double(0, 3.25) == 3.25);
    assert(overflow_int(0, 1, 2, 3, 4, 5, 91) == 91);
    assert(overflow_double(0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0,
                           9.5) == 9.5);
    return 0;
}
