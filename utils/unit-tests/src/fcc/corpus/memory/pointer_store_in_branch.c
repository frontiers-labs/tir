int printf(const char *format, ...);

/* A store through the caller's array in one arm of an `if`, where the other arm
   only loads from a local array and then overwrites what it loaded. The guard
   reads a value the loop carries out of its body; dropping either statement of
   the else arm hides the defect, so both stay. */
static int store_after_loop(int *p, int a, int b) {
    int loc[8] = {376, 902, 362, -484, -856, 339, 222, 863};

    for (int i = 0; i < 6; i++) {
        b = a;
    }
    if (b - loc[b & 7] < -156) {
        p[b & 3] = b >> 2;
    } else {
        b = loc[b & 7];
        b = -2088;
    }
    return a;
}

int main(void) {
    int arr[8] = {-87, -492, 8, -989, -561, -470, -429, 816};

    store_after_loop(arr + 4, -2816, 26624);
    for (int i = 0; i < 8; i++) {
        printf("%d\n", arr[i]);
    }
    return 0;
}
