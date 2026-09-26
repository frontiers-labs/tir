; RUN: not tir llvm-import %s 2>&1 | filecheck %s

define { i64, ptr } @partially_poison(i64 %value) {
  %result = insertvalue { i64, ptr } poison, i64 %value, 0
  ret { i64, ptr } %result
}

; CHECK: unsupported instruction: partially poison aggregate
