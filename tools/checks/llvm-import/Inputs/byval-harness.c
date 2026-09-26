#include <stdint.h>

struct S { uint64_t a, b, c, d; };
struct Small { uint64_t a; };
struct Pair { uint64_t a, b; };

extern uint64_t sum_byval(struct S value);
extern uint64_t call_sum_byval(struct S *value);
extern uint64_t call_mutate_small(struct Small *value, uint64_t later);
extern uint64_t call_clang_read_small(struct Small *value, uint64_t later);
extern uint64_t clang_call_tir_small(struct Small *value, uint64_t later);
extern uint64_t call_mutate_pair(struct Pair *value, uint64_t later);

int main(void) {
    struct S value = {11, 22, 33, 44};
    struct Small small = {11};
    struct Pair pair = {11, 22};
    return sum_byval(value) != 55 || call_sum_byval(&value) != 55
        || value.b != 22
        || call_mutate_small(&small, 5) != 104
        || call_clang_read_small(&small, 5) != 16
        || clang_call_tir_small(&small, 5) != 104
        || small.a != 11
        || call_mutate_pair(&pair, 5) != 82
        || pair.b != 22;
}
