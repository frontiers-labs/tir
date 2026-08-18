// RUN: fcc compile -std=c99 --stage preprocess -o - %s | filecheck %s
// RUN: fcc compile -std c99 --stage preprocess -o - %s | filecheck %s
// RUN: fcc compile --std=c99 --stage preprocess -o - %s | filecheck %s
// RUN: fcc compile --std c99 --stage preprocess -o - %s | filecheck %s

// The native CLI accepts every gcc spelling of the standard flag.

long version = __STDC_VERSION__;

// CHECK: long version = 199901L{{;}}
