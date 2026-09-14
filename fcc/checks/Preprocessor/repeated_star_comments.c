// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O0 -o /dev/null %s

/***/
#define VALUE 7
/****/
int main(void) {
    return VALUE != 7;
}
