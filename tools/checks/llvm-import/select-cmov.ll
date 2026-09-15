; RUN: tir mc --march=x86_64 --stage=isel %s llvm | filecheck %s

define i64 @choose(i64 %a, i64 %b, i64 %if_true, i64 %if_false) {
  %condition = icmp slt i64 %a, %b
  %result = select i1 %condition, i64 %if_true, i64 %if_false
  ret i64 %result
}

; CHECK: x86_64.cmp
; CHECK-NEXT: x86_64.cmovl
; CHECK-NOT: x86_64.setl
; CHECK-NOT: x86_64.and
; CHECK-NOT: x86_64.or
