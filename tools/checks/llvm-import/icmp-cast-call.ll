; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; icmp lowers to cmpi, the i1->i32 zext to extui, and the call takes the λ of
; the declaration reconstructed for @g.
define i32 @f(i32 %a, i32 %b) {
  %c = icmp slt i32 %a, %b
  %w = zext i1 %c to i32
  %r = call i32 @g(i32 %w)
  ret i32 %r
}

; CHECK: %{{[0-9]+}} = func.func @f
; CHECK: cmpi
; CHECK: extui
; CHECK: func.call %[[G:[0-9]+]](
; CHECK: func.return
; CHECK: %[[G]] = func.declare @g(!i32) -> !i32
