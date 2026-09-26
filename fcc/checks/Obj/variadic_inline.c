// RUN: fcc compile -O2 --stage obj --march x86_64 -o /tmp/fcc-variadic-inline.o %s
// RUN: cc /tmp/fcc-variadic-inline.o -o /tmp/fcc-variadic-inline
// RUN: /tmp/fcc-variadic-inline

static int side_effects;

static int bump(void) {
    return ++side_effects;
}

static int take_first(int value, ...) {
    return value;
}

int main(void) {
    int value = take_first(8, bump());
    return value != 8 || side_effects != 1;
}
