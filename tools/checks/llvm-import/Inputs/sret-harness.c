#include <stdint.h>

struct S { uint64_t a, b, c; };

extern struct S fill(uint64_t value);
extern uint64_t caller(uint64_t value);

int main(void) {
    struct S result = fill(UINT64_C(0x123456789abcdef0));
    return result.a != UINT64_C(0x123456789abcdef0) || caller(417) != 417;
}
