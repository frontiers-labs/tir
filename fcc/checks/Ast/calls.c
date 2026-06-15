// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/calls.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "apply" -> Int
// CHECK-NEXT:     Param "x": Int
// CHECK-NEXT:     Return
// CHECK-NEXT:       Add
// CHECK-NEXT:         Call "gcd"
// CHECK-NEXT:           Mul
// CHECK-NEXT:             Var "x"
// CHECK-NEXT:             Int 2
// CHECK-NEXT:           Int 48
// CHECK-NEXT:         Call "sum_to"
// CHECK-NEXT:           Var "x"
