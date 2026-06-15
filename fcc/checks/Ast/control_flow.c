// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/control_flow.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "classify" -> Int
// CHECK-NEXT:     Param "n": Int
// CHECK-NEXT:     Decl "sign": Int
// CHECK-NEXT:       Int 0
// CHECK-NEXT:     If
// CHECK-NEXT:       Lt
// CHECK-NEXT:         Var "n"
// CHECK-NEXT:         Int 0
// CHECK-NEXT:       Block
// CHECK-NEXT:         Assign "sign"
// CHECK-NEXT:           Neg
// CHECK-NEXT:             Int 1
// CHECK-NEXT:       If
// CHECK-NEXT:         Eq
// CHECK-NEXT:           Var "n"
// CHECK-NEXT:           Int 0
// CHECK-NEXT:         Block
// CHECK-NEXT:           Assign "sign"
// CHECK-NEXT:             Int 0
// CHECK-NEXT:         Block
// CHECK-NEXT:           Assign "sign"
// CHECK-NEXT:             Int 1
// CHECK-NEXT:     Return
// CHECK-NEXT:       Var "sign"
