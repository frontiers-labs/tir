// RUN: fcc compile --stage obj --march x86_64 -o /tmp/fcc-va-list-lit.o %s
// RUN: cc /tmp/fcc-va-list-lit.o %S/../Inputs/va_list_harness.c -o /tmp/fcc-va-list-lit
// RUN: /tmp/fcc-va-list-lit

#include <stdarg.h>

int first_unnamed(int fixed, ...) {
    va_list list;
    va_start(list, fixed);
    int result = va_arg(list, int);
    va_end(list);
    return result;
}

double first_double(int fixed, ...) {
    va_list list;
    va_start(list, fixed);
    double result = va_arg(list, double);
    va_end(list);
    return result;
}

int sixth_unnamed(int fixed, ...) {
    va_list list;
    va_start(list, fixed);
    int result = 0;
    for (int index = 0; index < 6; ++index)
        result = va_arg(list, int);
    va_end(list);
    return result;
}

double ninth_double(int fixed, ...) {
    va_list list;
    va_start(list, fixed);
    double result = 0;
    for (int index = 0; index < 9; ++index)
        result = va_arg(list, double);
    va_end(list);
    return result;
}

int after_seven(int a0, int a1, int a2, int a3, int a4, int a5, int a6, ...) {
    va_list list;
    va_start(list, a6);
    int result = va_arg(list, int);
    va_end(list);
    return result;
}

struct four_longs { long values[4]; };

int after_large_struct(struct four_longs fixed, ...) {
    va_list list;
    va_start(list, fixed);
    int result = 0;
    for (int index = 0; index < 7; ++index)
        result += va_arg(list, int);
    va_end(list);
    return result;
}

struct two_longs { long values[2]; };

int after_spilled_struct(int a0, int a1, int a2, int a3, int a4,
                         struct two_longs fixed, ...) {
    va_list list;
    va_start(list, fixed);
    int result = va_arg(list, int);
    va_end(list);
    return result;
}
