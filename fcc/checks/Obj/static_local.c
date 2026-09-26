// RUN: fcc compile --stage obj --march x86_64 -o /tmp/fcc-static-local-lit.o %s
// RUN: cc /tmp/fcc-static-local-lit.o -o /tmp/fcc-static-local-lit
// RUN: /tmp/fcc-static-local-lit

static int *first(void) {
    static int value = 7;
    return &value;
}

static int *second(void) {
    static int value = 11;
    return &value;
}

int main(void) {
    int *a = first();
    int *b = second();
    if (a == b || *a != 7 || *b != 11)
        return 1;
    ++*a;
    if (first() != a || *first() != 8 || *second() != 11)
        return 2;
    return 0;
}
