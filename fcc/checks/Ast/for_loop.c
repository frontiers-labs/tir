// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/for_loop.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "sum_to" -> Int
// CHECK-NEXT:     Param "n": Int
// CHECK-NEXT:     Decl "total": Int
// CHECK-NEXT:       Int 0
// CHECK-NEXT:     Decl "i": Int
// CHECK-NEXT:     For
// CHECK-NEXT:       Assign "i"
// CHECK-NEXT:         Int 1
// CHECK-NEXT:       Le
// CHECK-NEXT:         Var "i"
// CHECK-NEXT:         Var "n"
// CHECK-NEXT:       Assign "i"
// CHECK-NEXT:         Add
// CHECK-NEXT:           Var "i"
// CHECK-NEXT:           Int 1
// CHECK-NEXT:       Block
// CHECK-NEXT:         Assign "total"
// CHECK-NEXT:           Add
// CHECK-NEXT:             Var "total"
// CHECK-NEXT:             Var "i"
// CHECK-NEXT:         If
// CHECK-NEXT:           Gt
// CHECK-NEXT:             Var "total"
// CHECK-NEXT:             Int 100
// CHECK-NEXT:           Break
// CHECK-NEXT:     Return
// CHECK-NEXT:       Var "total"
