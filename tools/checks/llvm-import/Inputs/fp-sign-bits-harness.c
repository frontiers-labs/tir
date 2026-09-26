#include <stdint.h>
#include <string.h>

extern float negate_bits(float value);
extern float absolute_bits(float value);

static float from_bits(uint32_t bits) {
    float value;
    memcpy(&value, &bits, sizeof value);
    return value;
}

static uint32_t bits_of(float value) {
    uint32_t bits;
    memcpy(&bits, &value, sizeof bits);
    return bits;
}

int main(void) {
    uint32_t values[] = {0, 0x80000000u, 0x3f800000u, 0xbf800000u, 0xffc01234u};
    for (unsigned i = 0; i < sizeof values / sizeof values[0]; ++i) {
        float value = from_bits(values[i]);
        if (bits_of(negate_bits(value)) != (values[i] ^ 0x80000000u)) return 1;
        if (bits_of(absolute_bits(value)) != (values[i] & 0x7fffffffu)) return 2;
    }
    return 0;
}
