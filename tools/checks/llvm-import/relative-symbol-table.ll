; RUN: tir llvm-import %s | filecheck %s

@targets = private constant [2 x i32] [i32 trunc (i64 sub (i64 ptrtoint (ptr @left to i64), i64 ptrtoint (ptr @targets to i64)) to i32), i32 trunc (i64 sub (i64 ptrtoint (ptr @right to i64), i64 ptrtoint (ptr @targets to i64)) to i32)], align 4

; CHECK: global private @targets align 4 section ".rodata" bytes [0, 0, 0, 0, 0, 0, 0, 0] symbol_differences [{base = "targets", offset = 0, symbol = "left", width = 4}, {base = "targets", offset = 4, symbol = "right", width = 4}]
