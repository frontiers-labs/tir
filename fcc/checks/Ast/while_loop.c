// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/while_loop.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "gcd" -> Int
// CHECK-NEXT:     Param "a": Int
// CHECK-NEXT:     Param "b": Int
// CHECK-NEXT:     While
// CHECK-NEXT:       Ne
// CHECK-NEXT:         Var "b"
// CHECK-NEXT:         Int 0
// CHECK-NEXT:       Block
// CHECK-NEXT:         Decl "t": Int
// CHECK-NEXT:           Mod
// CHECK-NEXT:             Var "a"
// CHECK-NEXT:             Var "b"
// CHECK-NEXT:         Assign "a"
// CHECK-NEXT:           Var "b"
// CHECK-NEXT:         Assign "b"
// CHECK-NEXT:           Var "t"
// CHECK-NEXT:     Return
// CHECK-NEXT:       Var "a"
