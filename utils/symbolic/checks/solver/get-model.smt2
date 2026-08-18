; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 8))
(assert (= x #x2a))
(check-sat)
(get-model)

; CHECK: sat
; CHECK: (define-fun x () (_ BitVec 8) #x2a)
