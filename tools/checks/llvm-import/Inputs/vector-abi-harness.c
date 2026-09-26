#include <stdint.h>

typedef uint64_t vec2 __attribute__((vector_size(16)));

extern void store_ninth(vec2 *out, vec2 a0, vec2 a1, vec2 a2, vec2 a3,
                        vec2 a4, vec2 a5, vec2 a6, vec2 a7, vec2 a8);
extern void call_store_ninth(vec2 *out, const vec2 *source);
struct Out { vec2 first, second; uint64_t later; };
extern void store_tenth(struct Out *out, vec2 a0, vec2 a1, vec2 a2, vec2 a3,
                        vec2 a4, vec2 a5, vec2 a6, vec2 a7, vec2 a8, vec2 a9,
                        uint64_t later);
extern void call_store_tenth(struct Out *out, const vec2 *first,
                             const vec2 *second, uint64_t later);
extern void call_identity_vector(vec2 *out, const vec2 *source);

struct Mixed { vec2 value; double sum; };
extern void mixed_vector(struct Mixed *out, double first, vec2 value, double last);
extern void call_mixed_vector(struct Mixed *out, const vec2 *source, double first, double last);

int main(void) {
    vec2 source = {0x1122334455667788ULL, 0x99aabbccddeeff00ULL};
    vec2 zero = {0, 0};
    vec2 out = zero;
    store_ninth(&out, zero, zero, zero, zero, zero, zero, zero, zero, source);
    if (out[0] != source[0] || out[1] != source[1]) return 1;
    out = zero;
    call_store_ninth(&out, &source);
    if (out[0] != source[0] || out[1] != source[1]) return 2;
    vec2 other = {0x0123456789abcdefULL, 0xfedcba9876543210ULL};
    struct Out wide = {zero, zero, 0};
    store_tenth(&wide, zero, zero, zero, zero, zero, zero, zero, zero,
                source, other, 37);
    if (wide.first[0] != source[0] || wide.first[1] != source[1]
        || wide.second[0] != other[0] || wide.second[1] != other[1]
        || wide.later != 37) return 3;
    wide = (struct Out){zero, zero, 0};
    call_store_tenth(&wide, &source, &other, 37);
    if (wide.first[0] != source[0] || wide.first[1] != source[1]
        || wide.second[0] != other[0] || wide.second[1] != other[1]
        || wide.later != 37) return 4;
    out = zero;
    call_identity_vector(&out, &source);
    if (out[0] != source[0] || out[1] != source[1]) return 5;
    struct Mixed mixed = {zero, 0};
    mixed_vector(&mixed, 1.25, source, 2.5);
    if (mixed.value[0] != source[0] || mixed.value[1] != source[1]
        || mixed.sum != 3.75) return 6;
    mixed = (struct Mixed){zero, 0};
    call_mixed_vector(&mixed, &source, 1.25, 2.5);
    return mixed.value[0] != source[0] || mixed.value[1] != source[1]
        || mixed.sum != 3.75;
}
