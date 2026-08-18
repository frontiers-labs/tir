int printf(const char *format, ...);

int bump(int *counter, int value) {
    *counter = *counter + value;
    return *counter;
}

int classify(int value) {
    switch (value & 3) {
    case 0:
        return 10;
    case 1:
        return 20;
    case 2:
        break;
    default:
        return 30;
    }
    return 40;
}

int scan(int *items, int count, int wanted, int *counter) {
    int index;
    for (index = 0; index < count; index = index + 1) {
        if (items[index] == wanted) {
            return index;
        }
        bump(counter, 1);
    }
    return -1;
}

int consume(int limit, int *counter) {
    int total = 0;
    while (bump(counter, 1) < limit) {
        total = total + 1;
        if (total > 3) {
            return total;
        }
    }
    return -total;
}

int nested(int rows, int columns) {
    int row;
    int column;
    int seen = 0;
    for (row = 0; row < rows; row = row + 1) {
        column = 0;
        do {
            seen = seen + 1;
            if (row * column == 6) {
                return seen;
            }
            column = column + 1;
        } while (column < columns);
        if (seen > 50) {
            return -seen;
        }
    }
    return seen;
}

void report(int value) {
    if (value == 0) {
        printf("zero\n");
        return;
    }
    printf("value %d\n", value);
}

int main(void) {
    int items[5];
    int index;
    int counter;
    int found;
    for (index = 0; index < 5; index = index + 1) {
        items[index] = index * index;
    }

    printf("%d %d %d %d\n", classify(0), classify(1), classify(2), classify(3));
    counter = 0;
    found = scan(items, 5, 9, &counter);
    printf("%d %d\n", found, counter);
    counter = 0;
    found = scan(items, 5, 99, &counter);
    printf("%d %d\n", found, counter);
    counter = 0;
    found = consume(3, &counter);
    printf("%d %d\n", found, counter);
    counter = 0;
    found = consume(100, &counter);
    printf("%d %d\n", found, counter);
    printf("%d %d\n", nested(4, 4), nested(1, 2));
    report(0);
    report(7);
    counter = 0;
    return classify(1) + scan(items, 5, 4, &counter);
}
