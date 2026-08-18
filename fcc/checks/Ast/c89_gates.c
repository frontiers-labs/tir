/* RUN: not fcc compile --std c89 --stage ast -o - %s 2>&1 | filecheck %s

 Constructs from later standards are rejected under -std=c89.

 CHECK-DAG: [E0003] Error: long long integer type is unavailable in C89
 CHECK-DAG: [E0003] Error: declaration in for initializer is unavailable in C89
 CHECK-DAG: [E0003] Error: declaration after statement is unavailable in C89
*/
long long wide(void);
int main(void) {
    for (int index = 0; index < 1; index++) {}
    int late;
    return late;
}
