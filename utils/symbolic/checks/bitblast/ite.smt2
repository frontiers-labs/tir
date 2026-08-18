; ite selects by a boolean condition: the else-branch forces x >= 8.
; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 4))
(assert (= (ite (bvult x #x8) #x1 #x2) #x2))
(check-sat)
(get-value ((bvult x #x8)))

; CHECK: sat
; CHECK-NEXT: (((bvult x #x8) false))
