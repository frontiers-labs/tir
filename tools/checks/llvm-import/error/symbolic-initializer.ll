; RUN: not tir llvm-import %s

@unsupported = private constant i64 ptrtoint (ptr @target to i64)
