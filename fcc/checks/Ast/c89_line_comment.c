/* RUN: not fcc compile --std c89 --stage ast -o - %s 2>&1 | filecheck %s

 Line comments are diagnosed before parsing, so they get their own file.

 CHECK: [E0003] Error: line comment is unavailable in C89
*/
int main(void) {
    return 0;
}
// a C99 line comment
