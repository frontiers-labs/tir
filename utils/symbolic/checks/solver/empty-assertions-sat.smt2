; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 8))
(check-sat)

; CHECK: sat
