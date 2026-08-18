; zero_extend keeps the high bits zero; sign_extend copies the sign.
; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 4))
(push 1)
(assert (= ((_ zero_extend 4) x) #x0f))
(check-sat)
(pop 1)
(push 1)
(assert (= ((_ zero_extend 4) x) #xff))
(check-sat)
(pop 1)
(push 1)
(assert (= ((_ sign_extend 4) x) #xff))
(check-sat)
(pop 1)
(push 1)
(assert (= ((_ sign_extend 4) x) #x0f))
(check-sat)
(pop 1)

; CHECK: sat
; CHECK-NEXT: unsat
; CHECK-NEXT: sat
; CHECK-NEXT: unsat
