int main(void) {
    unsigned long composites = 0;
    int count = 0;
    int candidate;

    for (candidate = 2; candidate < 50; ++candidate) {
        if ((composites & (1UL << candidate)) == 0) {
            int multiple;
            ++count;
            for (multiple = candidate * candidate; multiple < 50; multiple += candidate) {
                composites |= 1UL << multiple;
            }
        }
    }

    return count == 15 ? 0 : 1;
}
