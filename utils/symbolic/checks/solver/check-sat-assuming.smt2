; Under `p` the formula is unsat; under `(not p)` it is sat.
; RUN: tir-smt %s | filecheck %s

(declare-const p Bool)
(declare-const q Bool)
(assert (=> p q))
(assert (not q))
(check-sat-assuming (p))
(check-sat-assuming ((not p)))

; CHECK: unsat
; CHECK-NEXT: sat
