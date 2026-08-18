; A 3-bit value cannot use `#x`; it must print as `#b`.
; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 3))
(assert (= x #b101))
(check-sat)
(get-value (x))

; CHECK: sat
; CHECK-NEXT: ((x #b101))
