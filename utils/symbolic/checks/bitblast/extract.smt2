; RUN: tir-smt %s | filecheck %s

(declare-const z (_ BitVec 8))
(assert (= ((_ extract 7 4) z) #xc))
(check-sat)
(get-value (((_ extract 7 4) z)))

; CHECK: sat
; CHECK-NEXT: #xc))
