; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

declare i64 @llvm.smax.i64(i64, i64)

define i64 @at_least_one(i64 %value) {
  %result = call i64 @llvm.smax.i64(i64 %value, i64 1)
  ret i64 %result
}

; CHECK-LABEL: func.func @at_least_one
; CHECK: cmpi {{.*}} {predicate = "sge"}
; CHECK: func.return
