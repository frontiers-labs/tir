int sum_to(int n) {
    int total = 0;
    int i;
    for (i = 1; i <= n; i = i + 1) {
        total = total + i;
        if (total > 100) break;
    }
    return total;
}
