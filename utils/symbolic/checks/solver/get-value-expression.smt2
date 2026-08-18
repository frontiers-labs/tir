; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 8))
(assert (= x #x10))
(check-sat)
(get-value ((bvadd x #x01)))

; CHECK: sat
; CHECK-NEXT: (((bvadd x #x01) #x11))
