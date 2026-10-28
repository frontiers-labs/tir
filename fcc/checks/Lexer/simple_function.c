// This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

// RUN: fcc compile --stage tokens -o - %S/../Inputs/simple_function.c | filecheck %s

// CHECK: [
// CHECK-NEXT:     KwInt,
// CHECK-NEXT:     Whitespace(
// CHECK-NEXT:         " ",
// CHECK-NEXT:     ),
// CHECK-NEXT:     Identifier(
// CHECK-NEXT:         "main",
// CHECK-NEXT:     ),
// CHECK-NEXT:     LParen,
// CHECK-NEXT:     RParen,
// CHECK-NEXT:     Whitespace(
// CHECK-NEXT:         " ",
// CHECK-NEXT:     ),
// CHECK-NEXT:     LBrace,
// CHECK-NEXT:     Whitespace(
// CHECK-NEXT:         " ",
// CHECK-NEXT:     ),
// CHECK-NEXT:     KwReturn,
// CHECK-NEXT:     Whitespace(
// CHECK-NEXT:         " ",
// CHECK-NEXT:     ),
// CHECK-NEXT:     IntegerLiteral(
// CHECK-NEXT:         "0",
// CHECK-NEXT:     ),
// CHECK-NEXT:     Semicolon,
// CHECK-NEXT:     Whitespace(
// CHECK-NEXT:         " ",
// CHECK-NEXT:     ),
// CHECK-NEXT:     RBrace,
// CHECK-NEXT:     Whitespace(
// CHECK-NEXT:         "\n",
// CHECK-NEXT:     ),
// CHECK-NEXT: ]
