; RUN: tir-smt %s | filecheck %s

(declare-const b Bool)
(assert b)
(check-sat)
(get-value (b))

; CHECK: sat
; CHECK-NEXT: ((b true))
