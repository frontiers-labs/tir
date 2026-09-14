// REQUIRES: linux, x86_64
// RUN: fcc compile --std gnu17 --stage ir --march x86_64 -O0 -o - %s | filecheck %s --check-prefix=IR
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O0 -o - %s | python3 %S/../Inputs/run_object.py
// RUN: fcc compile --std gnu17 --stage obj --march x86_64 -O2 -o - %s | python3 %S/../Inputs/run_object.py

void callback(void) {}

int function_is_null(void) {
    return callback == 0;
}

// IR-LABEL: func.func @function_is_null
// IR: %[[NULL:.*]] = ptr.null : !ptr.p
// IR: ptr.cmp %{{.*}}, %[[NULL]] {predicate = "eq"} : !i1

int check(int *pointer) {
    return (pointer == 0) + 2 * (0L == pointer) +
           4 * (pointer != (1 - 1)) + 8 * (0 != pointer);
}

// IR-LABEL: func.func @check
// IR: ptr.null : !ptr.p
// IR: ptr.cmp %{{.*}}, %{{.*}} {predicate = "eq"} : !i1

int main(void) {
    int value = 7;
    return check(&value) != 12 || check(0) != 3 || function_is_null();
}
