; RUN: tir mc --march=arm64 --filetype=asm %s llvm | filecheck %s

; A narrow constant under a widened compare is folded at the width the compare
; reads. Folded at its own 8 bits, -3 became the unsigned immediate 253 and the
; signed compare was always true.
define i1 @below_minus_three(i8 %x) {
  %r = icmp slt i8 %x, -3
  ret i1 %r
}

; CHECK-LABEL: below_minus_three:
; CHECK: movn x[[K:[0-9]+]], 2
; CHECK: asr x0, x0, 56
; CHECK-NEXT: cmp x0, x[[K]]
; CHECK-NEXT: cset x0, lt
