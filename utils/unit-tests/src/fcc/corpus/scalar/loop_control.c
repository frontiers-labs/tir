int printf(const char *format, ...);

int bump(int *counter, int value) {
    *counter = *counter + 1;
    return value;
}

int for_break(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        if (i == 4) {
            break;
        }
        total = total + i;
    }
    return total * 100 + i;
}

/* A `continue` skips the rest of the body but not the step, so the loop makes
   progress and `bump` runs only on the iterations that reach it. */
int for_continue(int n, int *counter) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        if (i % 3 == 0) {
            continue;
        }
        total = total + bump(counter, i);
    }
    return total * 100 + i;
}

/* A `continue` in a `do` reaches the condition, so the loop still ends. */
int do_continue(int n, int *counter) {
    int i = 0;
    int total = 0;
    do {
        i = i + 1;
        if (i % 2 == 0) {
            continue;
        }
        total = total + bump(counter, i);
    } while (i < n);
    return total * 100 + i;
}

/* An inner loop owns its own `continue` and `break`; the outer `for` keeps
   running its step for the `continue` that belongs to it. */
int nested(int n) {
    int i;
    int j;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        j = 0;
        while (j < n) {
            j = j + 1;
            if (j == 2) {
                continue;
            }
            if (j > 4) {
                break;
            }
            total = total + j;
        }
        if (i == 1) {
            continue;
        }
        total = total + 100;
    }
    return total * 10 + i;
}

/* A `continue` in a case leaves the switch as well as the rest of the body. */
int switch_continue(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        switch (i & 3) {
        case 0:
            continue;
        case 1:
            total = total + 1;
            break;
        default:
            total = total + 2;
        }
        total = total + 10;
    }
    return total;
}

int main(void) {
    int counter = 0;
    printf("%d\n", for_break(9));
    printf("%d\n", for_continue(10, &counter));
    printf("%d\n", counter);
    printf("%d\n", do_continue(9, &counter));
    printf("%d\n", counter);
    printf("%d\n", nested(4));
    printf("%d\n", switch_continue(9));
    return 0;
}
