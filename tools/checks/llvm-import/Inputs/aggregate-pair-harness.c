#include <stdint.h>

struct Pair {
    uint64_t first;
    void *second;
};

extern struct Pair pair(struct Pair *);
extern uint64_t first(struct Pair *);
extern struct Pair build_pair(uint64_t, void *);

int main(void) {
    int object = 5;
    struct Pair input = {UINT64_C(0x123456789abcdef0), &object};
    struct Pair result = pair(&input);
    struct Pair built = build_pair(input.first, input.second);
    return result.first != input.first || result.second != input.second ||
           first(&input) != input.first || built.first != input.first ||
           built.second != input.second;
}
