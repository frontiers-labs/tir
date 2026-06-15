// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/precedence.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "prec" -> Int
// CHECK-NEXT:     Param "a": Int
// CHECK-NEXT:     Param "b": Int
// CHECK-NEXT:     Param "c": Int
// CHECK-NEXT:     Return
// CHECK-NEXT:       LogOr
// CHECK-NEXT:         LogAnd
// CHECK-NEXT:           Lt
// CHECK-NEXT:             Sub
// CHECK-NEXT:               Add
// CHECK-NEXT:                 Var "a"
// CHECK-NEXT:                 Mul
// CHECK-NEXT:                   Var "b"
// CHECK-NEXT:                   Int 2
// CHECK-NEXT:               Mod
// CHECK-NEXT:                 Div
// CHECK-NEXT:                   Int 6
// CHECK-NEXT:                   Int 3
// CHECK-NEXT:                 Int 2
// CHECK-NEXT:             Var "c"
// CHECK-NEXT:           Not
// CHECK-NEXT:             Var "a"
// CHECK-NEXT:         Eq
// CHECK-NEXT:           Var "b"
// CHECK-NEXT:           Var "c"
