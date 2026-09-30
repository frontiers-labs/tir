; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; A switch is one branch on its value: each case and the default is an edge
; entered on the phi values it names, however many edges reach one block.

; CHECK: func.func @choose
; CHECK: cfg.switch %{{[0-9]+}} : !i32, [
; CHECK-NEXT: default: ^bb1(%{{[0-9]+}} : !i32),
; CHECK-NEXT: 0: ^bb1(%{{[0-9]+}} : !i32),
; CHECK-NEXT: -1: ^bb1(%{{[0-9]+}} : !i32)
; CHECK-NEXT: ]

define i32 @choose(i32 %x) {
entry.loop:
  switch i32 %x, label %done [
    i32 0, label %done
    i32 -1, label %done
  ]
done:
  %result = phi i32 [ 7, %entry.loop ]
  ret i32 %result
}
