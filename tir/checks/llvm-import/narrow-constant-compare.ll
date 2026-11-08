; IR migration: instruction selection and backend lowering are unavailable.
; XFAIL: *
; RUN: tir mc --march=arm64 --filetype=asm %s llvm | filecheck %s

; A narrow constant under a widened compare is folded at the width the compare
; reads. Folded at its own 8 bits, -3 became the unsigned immediate 253 and the
; signed compare was always true.
define i1 @below_minus_three(i8 %x) {
  %r = icmp slt i8 %x, -3
  ret i1 %r
}

; CHECK-LABEL: below_minus_three:
; CHECK: asr w[[X:[0-9]+]], w0, 24
; CHECK-NEXT: movz x[[K:[0-9]+]], -3
; CHECK: lslv x{{[0-9]+}}, x[[K]], x{{[0-9]+}}
; CHECK-NEXT: asr w[[C:[0-9]+]], w{{[0-9]+}}, 24
; CHECK-NEXT: cmp w[[X]], w[[C]]
; CHECK-NEXT: cset x0, lt
