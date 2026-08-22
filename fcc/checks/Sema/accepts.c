// RUN: fcc compile --std c17 --stage ir -o - %s | filecheck %s

// Well-formed programs sema must accept: record arguments for distinct tags,
// array-parameter adjustment, integer-constant-expression array bounds, enum
// subscripts and array-to-pointer decay in value contexts.

struct IntegerPair { long left; long right; };
struct MixedPair { double fp; long integer; };
long check_integer(struct IntegerPair value);
long check_mixed(struct MixedPair value);
int tagged_record_calls(void) {
    struct IntegerPair integers;
    struct MixedPair mixed;
    check_integer(integers);
    check_mixed(mixed);
    return 0;
}
// CHECK: %{{[0-9]+}} = func.func @tagged_record_calls

int entry(int argc, char **argv);
int entry(int argc, char *argv[]) { return argc; }
// CHECK: %{{[0-9]+}} = func.func @entry

enum Limits { NUM_CORE_STATES = 7, TOTAL_DATA_SIZE = 2000, MULTITHREAD = 1 };
int constant_bounds(void) {
    int counts[NUM_CORE_STATES];
    char data[TOTAL_DATA_SIZE * MULTITHREAD];
    return sizeof(counts) + sizeof(data);
}
// CHECK: %{{[0-9]+}} = func.func @constant_bounds

enum State { FIRST };
int enum_subscript(void) {
    int counts[1];
    enum State state = FIRST;
    counts[state]++;
    return counts[state];
}
// CHECK: %{{[0-9]+}} = func.func @enum_subscript

int consume(char *value);
char *global_values[1] = {(char *)"global"};
int array_decay(void) {
    char local[2];
    char *pointer;
    pointer = local;
    consume(local);
    return (char *)local == pointer;
}
// CHECK: %{{[0-9]+}} = func.func @array_decay
