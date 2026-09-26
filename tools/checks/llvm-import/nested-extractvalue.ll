; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

define i64 @nested_field({ { i64, i64 }, i64 } %value) {
  %result = extractvalue { { i64, i64 }, i64 } %value, 0, 1
  ret i64 %result
}

; CHECK-LABEL: func.func @nested_field
; CHECK: tuple_get {{.*}} {index = 0} : !tuple<!i64, !i64>
; CHECK: tuple_get {{.*}} {index = 1} : !i64
; CHECK: func.return
