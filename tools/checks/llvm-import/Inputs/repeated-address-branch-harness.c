struct pair { int *unused, *base; };
extern int call_branch(struct pair *value, long index);

int main(void) {
    int values[] = {0, 0, 7};
    struct pair value = {0, values};
    if (call_branch(&value, 1) != 0 || call_branch(&value, 2) != 2) return 1;
    values[0] = 3;
    return call_branch(&value, 1) != 1;
}
