; RUN: not tir llvm-import %s 2>&1 | filecheck %s

@values = global [2 x i16] [i16 1]

; CHECK: failed to parse LLVM IR: global @values initializer has 2 bytes, expected 4
