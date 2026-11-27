; IR migration: instruction selection and backend lowering are unavailable.
; XFAIL: *
; RUN: tir llvm-import %s | tir opt --verify | filecheck %s
; RUN: tir mc --march=x86_64 --filetype=obj -o /dev/null %s

define i64 @add_signed(i32 %x) {
  %r = add nsw i32 %x, 5
  %w = sext i32 %r to i64
  ret i64 %w
}

define i64 @sub_left(i16 %x) {
  %r = sub nsw i16 7, %x
  %w = sext i16 %r to i64
  ret i64 %w
}

define i64 @negative_constant(i8 %x) {
  %r = add nsw i8 %x, 255
  %w = sext i8 %r to i64
  ret i64 %w
}

define i64 @wrapping(i8 %x) {
  %r = add i8 %x, 1
  %w = sext i8 %r to i64
  ret i64 %w
}

define i64 @unsigned_only(i8 %x) {
  %r = add nuw i8 %x, 1
  %w = sext i8 %r to i64
  ret i64 %w
}

define i64 @cross_block(i32 %x) {
entry:
  %r = add nuw nsw i32 %x, 5
  br label %next
next:
  %w = sext i32 %r to i64
  ret i64 %w
}

define i64 @mixed_uses(i32 %x, ptr %out) {
  %r = add nsw i32 %x, 5
  store i32 %r, ptr %out
  %w = sext i32 %r to i64
  ret i64 %w
}

define i64 @zero_extended(i8 %x) {
  %r = add nsw i8 %x, 1
  %w = zext i8 %r to i64
  ret i64 %w
}

; CHECK-LABEL: func.func @add_signed(%[[X:[0-9]+]]: !i32)
; CHECK: %[[A:[0-9]+]] = extsi %[[X]] : !i64
; CHECK: %[[B:[0-9]+]] = extsi %{{[0-9]+}} : !i64
; CHECK: %[[SUM:[0-9]+]] = addi %[[A]], %[[B]] : !i64
; CHECK: func.return %[[SUM]]
; CHECK-LABEL: func.func @sub_left(%[[X:[0-9]+]]: !i16)
; CHECK: %[[A:[0-9]+]] = extsi %{{[0-9]+}} : !i64
; CHECK: %[[B:[0-9]+]] = extsi %[[X]] : !i64
; CHECK: %[[DIFF:[0-9]+]] = subi %[[A]], %[[B]] : !i64
; CHECK: func.return %[[DIFF]]
; CHECK-LABEL: func.func @negative_constant(%[[X:[0-9]+]]: !i8)
; CHECK: %[[C:[0-9]+]] = constant {value = 255} : !i8
; CHECK: %[[A:[0-9]+]] = extsi %[[X]] : !i64
; CHECK: %[[B:[0-9]+]] = extsi %{{[0-9]+}} : !i64
; CHECK: addi %[[A]], %[[B]] : !i64
; CHECK-LABEL: func.func @wrapping
; CHECK: %[[N:[0-9]+]] = addi %{{[0-9]+}}, %{{[0-9]+}} : !i8
; CHECK: extsi %[[N]] : !i64
; CHECK-LABEL: func.func @unsigned_only
; CHECK: %[[N:[0-9]+]] = addi %{{[0-9]+}}, %{{[0-9]+}} : !i8
; CHECK: extsi %[[N]] : !i64
; CHECK-LABEL: func.func @cross_block(%[[X:[0-9]+]]: !i32)
; CHECK: cfg.br
; CHECK: extsi %[[X]] : !i64
; CHECK: addi %{{[0-9]+}}, %{{[0-9]+}} : !i64
; CHECK-LABEL: func.func @mixed_uses(%[[X:[0-9]+]]: !i32,
; CHECK: %[[N:[0-9]+]] = addi %{{[0-9]+}}, %{{[0-9]+}} : !i32
; CHECK: ptr.store %[[N]],
; CHECK: extsi %[[X]] : !i64
; CHECK: addi %{{[0-9]+}}, %{{[0-9]+}} : !i64
; CHECK-LABEL: func.func @zero_extended
; CHECK: %[[N:[0-9]+]] = addi %{{[0-9]+}}, %{{[0-9]+}} : !i8
; CHECK: extui %[[N]] : !i64
