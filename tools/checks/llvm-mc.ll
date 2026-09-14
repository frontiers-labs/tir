; RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s
; RUN: cat %s | tir mc --march=x86_64 --filetype=obj-ascii - llvm | filecheck %s

; CHECK: answer

define i32 @answer() {
  ret i32 42
}
