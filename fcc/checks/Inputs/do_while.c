int countdown(int n) {
    int i = 0;
    do {
        i = i + 1;
        if (i == 3) continue;
    } while (i < n);
    return i;
}
