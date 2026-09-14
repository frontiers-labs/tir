// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --std gnu17 --stage preprocess -o - %S/../Inputs/repeated_star_comments.c | filecheck %s

// CHECK: /***/
// CHECK-NEXT: /****/
// CHECK-EMPTY:
// CHECK: /****************************************************************************/
// CHECK-NEXT: /*                   DHRYSTONE BENCHMARK PREPROCESSOR TEST                  */
// CHECK-NEXT: /****************************************************************************/
// CHECK-NEXT: /****************************************************************************/
// CHECK-EMPTY:
// CHECK-EMPTY:
// CHECK: int main(void) {
// CHECK-NEXT:     return 7 != 7 || 1 != 1;
// CHECK-NEXT: }
