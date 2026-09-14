// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O0 -o /dev/null %s
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O2 -o /dev/null %s

int check(int *pointer) {
    return (pointer == 0) + 2 * (0L == pointer) +
           4 * (pointer != (1 - 1)) + 8 * (0 != pointer);
}

int main(void) {
    int value = 7;
    return check(&value) != 12 || check(0) != 3;
}
