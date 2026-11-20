; IR migration: instruction selection and backend lowering are unavailable.
; XFAIL: *
; RUN: tir llvm-import %s | tir opt --verify | filecheck %s
; RUN: tir mc --march=x86_64 --filetype=obj -o /dev/null %s

@items = global [4 x i32] zeroinitializer

define ptr @inline_zero() {
  ret ptr getelementptr ([4 x i32], ptr @items, i64 0, i64 0)
}

; CHECK: %[[BASE:[0-9]+]] = global @items
; CHECK-LABEL: func.func @inline_zero
; CHECK-NOT: ptr.ptradd
; CHECK: func.return %[[BASE]]
