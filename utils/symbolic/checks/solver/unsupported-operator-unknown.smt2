; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 4))
(declare-const y (_ BitVec 4))
(assert (= (bvsmod x y) x))
(check-sat)

; CHECK: unknown
