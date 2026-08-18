int main(void) {
    int value = 17;
    int *pointer = &value;
    *pointer = 42;
    return value == 42 ? 0 : 1;
}
