extern int choose(int value);
extern int choose_implicit(int value);

int main(void) {
    return choose(0) != 7 || choose(1) != 7 || choose(9) != 7 ||
           choose_implicit(1) != 7 || choose_implicit(2) != 7 ||
           choose_implicit(9) != 7;
}
