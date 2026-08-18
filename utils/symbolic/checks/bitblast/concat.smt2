; concat: x is the high nibble, y the low nibble.
; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 4))
(declare-const y (_ BitVec 4))
(assert (= (concat x y) #xab))
(check-sat)
(get-value (x y))

; CHECK: sat
; CHECK-NEXT: ((x #xa) (y #xb))
