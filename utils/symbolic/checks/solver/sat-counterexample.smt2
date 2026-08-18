; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 8))
(assert (= (bvadd x #x01) #x00))
(check-sat)
(get-value (x))

; CHECK: sat
; CHECK-NEXT: ((x #xff))
