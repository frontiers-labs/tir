; RUN: tir llvm-import %s | tir opt --verify | filecheck %s
; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-switch-dotted-label-lit.o
; RUN: cc /tmp/tir-switch-dotted-label-lit.o %S/Inputs/switch-dotted-label-harness.c -o /tmp/tir-switch-dotted-label-lit
; RUN: /tmp/tir-switch-dotted-label-lit

; CHECK: func.func @choose
; CHECK: constant {value = 7}
; CHECK: cfg.cond_br

define i32 @choose(i32 %x) {
entry.loop:
  switch i32 %x, label %done [
    i32 0, label %done
    i32 1, label %done
  ]
done:
  %result = phi i32 [ 7, %entry.loop ]
  ret i32 %result
}

define i32 @choose_implicit(i32 %0) {
  switch i32 %0, label %done [
    i32 1, label %done
    i32 2, label %done
  ]
done:
  %result = phi i32 [ 7, %1 ]
  ret i32 %result
}

; CHECK-LABEL: func.func @choose_implicit
; CHECK: cfg.cond_br
