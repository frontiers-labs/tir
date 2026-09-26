extern int call_with_poison(void);

int main(void) {
    return call_with_poison() != 7;
}
