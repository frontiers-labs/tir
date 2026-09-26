#include <stdint.h>
#include <stdio.h>
#include <string.h>

extern double from_unsigned64(uint64_t);

static int check(uint64_t value) {
    double expected = (double)value;
    double actual = from_unsigned64(value);
    if (memcmp(&expected, &actual, sizeof(actual)) == 0)
        return 0;
    fprintf(stderr, "%llu: expected %a, got %a\n",
            (unsigned long long)value, expected, actual);
    return 1;
}

int main(void) {
    // Include both sides of every exponent boundary and the rounding midpoint
    // between representable doubles. Odd upper-half inputs exercise the sticky bit.
    if (check(0) || check(UINT64_MAX))
        return 1;
    for (unsigned bit = 0; bit < 64; ++bit) {
        uint64_t power = UINT64_C(1) << bit;
        if (check(power - 1) || check(power) || check(power + 1))
            return 1;
        if (bit > 52) {
            uint64_t midpoint = power + (UINT64_C(1) << (bit - 53));
            if (check(midpoint - 1) || check(midpoint) || check(midpoint + 1))
                return 1;
        }
    }
    for (uint64_t delta = 0; delta < 4096; ++delta)
        if (check((UINT64_C(1) << 63) - delta) ||
            check((UINT64_C(1) << 63) + delta) || check(UINT64_MAX - delta))
            return 1;
    return 0;
}
