; RUN: tir llvm-import %s | tir opt --verify | filecheck %s
; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-sret-lit.o
; RUN: cc /tmp/tir-sret-lit.o %S/Inputs/sret-harness.c -o /tmp/tir-sret-lit
; RUN: /tmp/tir-sret-lit

%S = type { i64, i64, i64 }

define void @fill(ptr sret(%S) align 8 %out, i64 %value) {
  %field = getelementptr %S, ptr %out, i32 0, i32 0
  store i64 %value, ptr %field, align 8
  ret void
}

; CHECK: func.func @fill
; CHECK: result_address

define i64 @caller(i64 %value) {
  %slot = alloca %S, align 8
  call void @fill(ptr sret(%S) align 8 %slot, i64 %value)
  %field = getelementptr %S, ptr %slot, i32 0, i32 0
  %result = load i64, ptr %field, align 8
  ret i64 %result
}

; CHECK: func.func @caller
; CHECK: func.call
