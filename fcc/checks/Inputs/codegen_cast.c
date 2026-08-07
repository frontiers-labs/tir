int truncate(int value) {
    return (unsigned char)value;
}

long widen(int value) {
    return (long)value;
}

unsigned long widen_unsigned(unsigned int value) {
    return (unsigned long)value;
}

void *null_pointer(void) {
    return 0;
}

void *negative_pointer(void) {
    return (void *)-1;
}

struct Node {
    struct Node *next;
};

void clear(struct Node *node) {
    node->next = 0;
}
