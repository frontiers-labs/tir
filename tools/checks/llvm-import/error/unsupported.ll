; RUN: not tir llvm-import %s

; Floating remainder has no importer lowering, so it must fail explicitly.
define double @d(double %a, double %b) {
  %r = frem double %a, %b
  ret double %r
}
