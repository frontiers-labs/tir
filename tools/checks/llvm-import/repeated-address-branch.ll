; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-repeated-address-branch-lit.o
; RUN: cc /tmp/tir-repeated-address-branch-lit.o %S/Inputs/repeated-address-branch-harness.c -o /tmp/tir-repeated-address-branch-lit
; RUN: /tmp/tir-repeated-address-branch-lit

target triple = "x86_64-pc-linux-gnu"
%s = type {ptr, ptr}
define i32 @f(ptr byval(%s) %p, i64 %i) {
entry:
  %field = getelementptr %s, ptr %p, i32 0, i32 1
  %base = load ptr, ptr %field
  %q = getelementptr i32, ptr %base, i64 %i
  %v = load i32, ptr %q
  %a = icmp eq i32 %v, 0
  br i1 %a, label %outer, label %exit
outer:
  %field2 = getelementptr %s, ptr %p, i32 0, i32 1
  %base2 = load ptr, ptr %field2
  %w = load i32, ptr %base2
  %b = icmp eq i32 %w, 0
  br i1 %b, label %yes, label %no
yes:
  ret i32 0
no:
  ret i32 1
exit:
  ret i32 2
}

define i32 @call_branch(ptr %p, i64 %i) {
  %result = call i32 @f(ptr byval(%s) %p, i64 %i)
  ret i32 %result
}
