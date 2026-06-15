// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/sum.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "sum" -> Int
// CHECK-NEXT:     Param "a": Int
// CHECK-NEXT:     Param "b": Int
// CHECK-NEXT:     Return
// CHECK-NEXT:       Add
// CHECK-NEXT:         Var "a"
// CHECK-NEXT:         Var "b"
