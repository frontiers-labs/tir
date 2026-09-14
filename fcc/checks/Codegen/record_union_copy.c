// REQUIRES: linux, x86_64
// RUN: fcc compile --march x86_64 --stage ir -o - %s | filecheck %s
// RUN: fcc compile --march x86_64 -O0 --stage obj -o - %s | python3 %S/../Inputs/run_object.py
// RUN: fcc compile --march x86_64 -O2 --stage obj -o - %s | python3 %S/../Inputs/run_object.py

struct record {
    struct record *next;
    int discriminator;
    union {
        struct { int enumeration; int integer; char text[31]; } first;
        struct { int enumeration; char text[31]; } second;
        struct { char first; char second; } third;
    } variant;
};

struct record source;
struct record destination;

void copy(struct record *to, struct record *from) {
    *to = *from;
}

int main(void) {
    source.next = &source;
    source.discriminator = 1;
    source.variant.first.enumeration = 2;
    source.variant.first.integer = 37;
    for (int i = 0; i < 31; i++)
        source.variant.first.text[i] = 'A' + i % 26;
    copy(&destination, &source);
    if (destination.next != &source || destination.discriminator != 1 ||
        destination.variant.first.enumeration != 2 || destination.variant.first.integer != 37)
        return 1;
    for (int i = 0; i < 31; i++)
        if (destination.variant.first.text[i] != 'A' + i % 26)
            return 2;
    source.variant.second.enumeration = 3;
    for (int i = 0; i < 31; i++)
        source.variant.second.text[i] = 'a' + i % 26;
    copy(&destination, &source);
    if (destination.variant.second.enumeration != 3)
        return 3;
    for (int i = 0; i < 31; i++)
        if (destination.variant.second.text[i] != 'a' + i % 26)
            return 4;
    return 0;
}

// CHECK-LABEL: func.func @copy
// CHECK: %[[SIZE:[0-9]+]] = constant {value = 56} : !i64
// CHECK: ptr.memcpy {{.*}}, %[[SIZE]]
// CHECK-NOT: ptr.memcpy
// CHECK-NOT: cir.
// CHECK-LABEL: func.func @main
