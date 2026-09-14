; RUN: tir mc %s --march x86_64 --filetype obj-ascii | filecheck %s

define i32 @convert(double %value) {
  %result = fptoui double %value to i32
  ret i32 %result
}

; CHECK: .section .text
; CHECK: convert:
