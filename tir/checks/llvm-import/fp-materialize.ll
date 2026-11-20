; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

define float @multiply_add(float %a, float %b, float %c) {
  %product = fmul float %a, %b
  %sum = fadd float %product, %c
  ret float %sum
}

; CHECK-LABEL: func.func @multiply_add(%[[A:[0-9]+]]: !f32, %[[B:[0-9]+]]: !f32, %[[C:[0-9]+]]: !f32) -> !f32
; CHECK: %[[X:[0-9]+]] = fp.capture %[[A]] : !fp.computation<!f32, nearest_even, separate>
; CHECK-NEXT: %[[Y:[0-9]+]] = fp.capture %[[B]] : !fp.computation<!f32, nearest_even, separate>
; CHECK-NEXT: %[[MUL:[0-9]+]] = fp.mul %[[X]], %[[Y]] : !fp.computation<!f32, nearest_even, separate>
; CHECK-NEXT: %[[PRODUCT:[0-9]+]] = fp.materialize %[[MUL]] : !f32
; CHECK-NEXT: %[[P:[0-9]+]] = fp.capture %[[PRODUCT]] : !fp.computation<!f32, nearest_even, separate>
; CHECK-NEXT: %[[Z:[0-9]+]] = fp.capture %[[C]] : !fp.computation<!f32, nearest_even, separate>
; CHECK-NEXT: %[[ADD:[0-9]+]] = fp.add %[[P]], %[[Z]] : !fp.computation<!f32, nearest_even, separate>
; CHECK-NEXT: %[[SUM:[0-9]+]] = fp.materialize %[[ADD]] : !f32
; CHECK: func.return %[[SUM]]
