int printf(const char *format, ...);

/* The cleanup exit: every failure path leaves through one label, which is the
   shape CoreMark-style resource handling is written in. */
int acquire(int step, int fail, int *released) {
    int first = 0;
    int second = 0;
    int status = 0;
    if (step < 1) {
        status = 1;
        goto done;
    }
    first = 1;
    if (fail == 1) {
        status = 2;
        goto release_first;
    }
    if (step < 2) {
        status = 3;
        goto release_first;
    }
    second = 1;
    if (fail == 2) {
        status = 4;
        goto release_both;
    }
    status = 0;
release_both:
    if (second) {
        *released = *released + 2;
    }
release_first:
    if (first) {
        *released = *released + 1;
    }
done:
    return status;
}

/* Two `goto`s reach one label from different depths. */
int first_hit(int a, int b) {
    int found = 0;
    if (a > 2) {
        found = 1;
        goto out;
    }
    if (b > 2) {
        found = 2;
        goto out;
    }
    found = 3;
out:
    return found * 10 + a + b;
}

/* A `goto` out of nested loops leaves both. */
int scan(int n) {
    int i;
    int j = 0;
    for (i = 0; i < n; i = i + 1) {
        for (j = 0; j < n; j = j + 1) {
            if (i * j > 6) {
                goto found;
            }
        }
    }
    i = -1;
found:
    return i * 100 + j;
}

/* A loop written out of a backward `goto`. */
int accumulate(int n) {
    int total = 0;
    int i = 0;
again:
    if (i >= n) {
        goto done;
    }
    total = total + i;
    i = i + 1;
    goto again;
done:
    return total;
}

/* Two labels reached backwards from each other: an irreducible region. */
int alternate(int n) {
    int total = 0;
odd:
    if (n <= 0) {
        goto done;
    }
    total = total + 1;
    n = n - 1;
    goto even;
even:
    if (n <= 0) {
        goto done;
    }
    total = total + 10;
    n = n - 1;
    goto odd;
done:
    return total;
}

/* A jump past a declaration into a compound statement. */
int into_block(int enter) {
    int total = 0;
    if (enter) {
        goto inside;
    }
    {
        int local = 5;
        total = total + local;
    inside:
        total = total + 1;
    }
    return total;
}

/* A jump into a loop body enters the loop without asking its condition. */
int into_loop(int enter) {
    int value = 0;
    int total = 0;
    if (enter) {
        goto inside;
    }
    while (value < 2) {
        total = total + 10;
    inside:
        total = total + 1;
        value = value + 1;
    }
    return total;
}

/* A jump into the body of a `for` whose body also returns early: the loop is
   entered without its init or its condition, and leaves through the flag. */
int into_for(int enter, int stop) {
    int i = 0;
    int total = 0;
    if (enter) {
        goto middle;
    }
    for (i = 0; i < 4; i = i + 1) {
        total = total + 100;
    middle:
        total = total + 1;
        if (total > stop) {
            return -total;
        }
    }
    return total;
}

/* A `goto` out of a `switch` case stops the switch. */
int classify(int n) {
    int total = 0;
    switch (n) {
    case 0:
        total = 1;
        goto done;
    case 1:
        total = 2;
        break;
    default:
        total = 3;
    }
    total = total + 10;
done:
    return total;
}

/* A `goto` into a `switch` body lands in a case without asking the
   controlling expression, and falls through from there. */
int dispatch(int n, int enter) {
    int total = 0;
    if (enter) {
        goto inside;
    }
    switch (n) {
    case 0:
        total = total + 1;
    case 1:
        total = total + 10;
    inside:
        total = total + 100;
        break;
    default:
        total = total + 1000;
    }
    return total;
}

/* A `switch` inside a loop, whose arms leave through every exit C has: a
   `goto` past the rest of the iteration, a `continue`, a `break` out of the
   switch and fallthrough. */
int mix(int n) {
    int total = 0;
    int i;
    for (i = 0; i < n; i = i + 1) {
        switch (i & 3) {
        case 0:
            goto skip;
        case 1:
            continue;
        case 2:
            total = total + 5;
            break;
        default:
            total = total + 1;
        }
        total = total + 100;
    skip:
        total = total + 1000;
    }
    return total;
}

/* A `goto` inside a loop that stays in the loop, next to `continue`. */
int retry(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        int guard = 0;
    step:
        total = total + 1;
        if (guard == 0 && (i & 1)) {
            guard = 1;
            goto step;
        }
        if (i == 3) {
            continue;
        }
        total = total + 100;
    }
    return total;
}

/* `break` and `continue` for the enclosing loop from inside the dispatch loop
   a backward `goto` builds. */
int mesh(int n) {
    int i;
    int total = 0;
    for (i = 0; i < n; i = i + 1) {
        int tries = 0;
    retry:
        total = total + 1;
        if (total > 50) {
            break;
        }
        if (i == 2 && tries == 1) {
            continue;
        }
        if (tries == 0 && (i & 1)) {
            tries = 1;
            goto retry;
        }
        total = total + 10;
    }
    return total * 10 + i;
}

int main(void) {
    int released = 0;
    printf("%d\n", acquire(0, 0, &released));
    printf("%d\n", acquire(1, 1, &released));
    printf("%d\n", acquire(1, 0, &released));
    printf("%d\n", acquire(2, 2, &released));
    printf("%d\n", acquire(2, 0, &released));
    printf("%d\n", released);
    printf("%d\n", first_hit(1, 1));
    printf("%d\n", first_hit(3, 1));
    printf("%d\n", first_hit(1, 3));
    printf("%d\n", scan(5));
    printf("%d\n", scan(2));
    printf("%d\n", accumulate(6));
    printf("%d\n", alternate(5));
    printf("%d\n", into_block(0));
    printf("%d\n", into_block(1));
    printf("%d\n", into_loop(0));
    printf("%d\n", into_loop(1));
    printf("%d\n", into_for(0, 1000));
    printf("%d\n", into_for(1, 1000));
    printf("%d\n", into_for(0, 250));
    printf("%d\n", into_for(1, 1));
    printf("%d\n", classify(0));
    printf("%d\n", classify(1));
    printf("%d\n", classify(7));
    printf("%d\n", dispatch(0, 0));
    printf("%d\n", dispatch(1, 0));
    printf("%d\n", dispatch(2, 0));
    printf("%d\n", dispatch(0, 1));
    printf("%d\n", dispatch(5, 1));
    printf("%d\n", mix(4));
    printf("%d\n", mix(9));
    printf("%d\n", retry(6));
    printf("%d\n", mesh(5));
    printf("%d\n", mesh(30));
    return 0;
}
