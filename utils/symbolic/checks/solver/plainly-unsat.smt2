; RUN: tir-smt %s | filecheck %s

(declare-const x (_ BitVec 8))
(assert (and (bvult x #x05) (bvugt x #x05)))
(check-sat)

; CHECK: unsat
