; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 4))
(assert (bvult x #x5))
(check-sat)
(push 1)
(assert (bvugt x #x5))
(check-sat)
(pop 1)
(check-sat)

; CHECK: sat
; CHECK-NEXT: unsat
; CHECK-NEXT: sat
