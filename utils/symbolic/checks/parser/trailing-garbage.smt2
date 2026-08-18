; RUN: not tir-smt %s 2>&1 | filecheck %s

(check-sat) extra

; CHECK: error:
