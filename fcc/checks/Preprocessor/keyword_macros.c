// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --std gnu17 --stage preprocess -o - %S/../Inputs/keyword_macros.c | filecheck %s

// CHECK: /* Keywords are preprocessing identifiers until expansion finishes. */
// CHECK-EMPTY:
// CHECK-EMPTY:
// CHECK: int main(void) {
// CHECK-NEXT:     return 0 || !(!0) || 7 != 7;
// CHECK-NEXT: }
