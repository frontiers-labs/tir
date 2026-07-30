int bar(void);
int baz(void);

char *labels[] = {"first", "second", "third"};

int classify(int k, double scale) {
    int total = (int)(scale * 2.0);
    if (k > 0 && !bar()) {
        total ^= k;
    }
    return total;
}

struct foo {
    struct foo *next;
};

struct foo *test(struct foo *node) {
    while (node) {
        if (bar() && !baz()) {
            break;
        }
        node = node->next;
    }
    return node;
}
