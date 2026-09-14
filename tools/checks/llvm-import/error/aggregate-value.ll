; RUN: not tir llvm-import %s 2>&1 | filecheck %s

define void @aggregate_value(ptr %source) {
  %value = load [4 x i8], ptr %source
  ret void
}

; CHECK: unsupported instruction: LLVM array value
