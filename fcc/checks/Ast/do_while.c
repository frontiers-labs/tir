// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage ast -o - %S/../Inputs/do_while.c | filecheck %s

// CHECK: TranslationUnit
// CHECK-NEXT:   Function "countdown" -> Int
// CHECK-NEXT:     Param "n": Int
// CHECK-NEXT:     Decl "i": Int
// CHECK-NEXT:       Int 0
// CHECK-NEXT:     DoWhile
// CHECK-NEXT:       Block
// CHECK-NEXT:         Assign "i"
// CHECK-NEXT:           Add
// CHECK-NEXT:             Var "i"
// CHECK-NEXT:             Int 1
// CHECK-NEXT:         If
// CHECK-NEXT:           Eq
// CHECK-NEXT:             Var "i"
// CHECK-NEXT:             Int 3
// CHECK-NEXT:           Continue
// CHECK-NEXT:       Lt
// CHECK-NEXT:         Var "i"
// CHECK-NEXT:         Var "n"
// CHECK-NEXT:     Return
// CHECK-NEXT:       Var "i"
