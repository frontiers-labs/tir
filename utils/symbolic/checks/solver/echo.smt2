; RUN: tir-smt %s | filecheck %s

(echo "hello")

; CHECK: "hello"
