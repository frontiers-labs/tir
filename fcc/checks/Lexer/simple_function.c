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
// CHECK-NEXT:         IntegerLiteral {
// CHECK-NEXT:             value: APInt {
// CHECK-NEXT:                 width: 1,
// CHECK-NEXT:                 signed: false,
// CHECK-NEXT:                 value: 0,
// CHECK-NEXT:                 high: None,
// CHECK-NEXT:             },
// CHECK-NEXT:             spelling: "0",
// CHECK-NEXT:         },
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
