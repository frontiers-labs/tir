; RUN: tir llvm-import %s | tir opt --verify | filecheck %s
; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-aggregate-pair-lit.o
; RUN: cc /tmp/tir-aggregate-pair-lit.o %S/Inputs/aggregate-pair-harness.c -o /tmp/tir-aggregate-pair-lit
; RUN: /tmp/tir-aggregate-pair-lit

define { i64, ptr } @pair(ptr %p) {
  %value = load { i64, ptr }, ptr %p, align 8
  ret { i64, ptr } %value
}

; CHECK: func.func @pair
; CHECK: make_tuple

define i64 @first(ptr %p) {
  %value = load { i64, ptr }, ptr %p, align 8
  %result = extractvalue { i64, ptr } %value, 0
  ret i64 %result
}

; CHECK: func.func @first

define { i64, ptr } @build_pair(i64 %first, ptr %second) {
  %partial = insertvalue { i64, ptr } poison, i64 %first, 0
  %complete = insertvalue { i64, ptr } %partial, ptr %second, 1
  ret { i64, ptr } %complete
}

; CHECK: func.func @build_pair
; CHECK: make_tuple
