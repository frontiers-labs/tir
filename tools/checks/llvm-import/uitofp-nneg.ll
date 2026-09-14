; RUN: tir mc %s --march x86_64 --filetype obj-ascii | filecheck %s

define float @convert(i64 %value) {
  %result = uitofp nneg i64 %value to float
  ret float %result
}

; CHECK: .section .text
; CHECK: convert:
