unsigned long mix(unsigned long value, unsigned int round) {
    unsigned int amount = (round & 7U) + 1U;
    value ^= value >> amount;
    value *= 1664525UL;
    value += 1013904223UL + (unsigned long)round;
    return value;
}

unsigned int fold(unsigned long value) {
    return (unsigned int)((value ^ (value >> 32)) & 255UL);
}

int main(void) {
    unsigned long state = 0x12345678UL;
    unsigned int round;

    for (round = 0; round < 24U; ++round) {
        state = mix(state, round);
    }

    return (int)fold(state);
}
