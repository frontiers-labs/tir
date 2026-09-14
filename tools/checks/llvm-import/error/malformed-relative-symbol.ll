; RUN: not tir llvm-import %s

@target = private constant [1 x i32] [i32 trunc (i64 add (i64 sub (i64 ptrtoint (ptr @left to i64), i64 ptrtoint (ptr @target to i64)), i64 4) to i32)], align 4
