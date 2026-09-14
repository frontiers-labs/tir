; RUN: not tir llvm-import %s 2>&1 | filecheck %s

@message = private constant [4 x i8] c"abcde"

; CHECK: failed to parse LLVM IR: global @message initializer has 5 bytes, expected 4
