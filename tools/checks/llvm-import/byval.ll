; RUN: tir llvm-import %s | tir mc --march x86_64 --filetype obj - tir -o /tmp/tir-byval-lit.o
; RUN: clang -c %S/Inputs/byval-external.ll -o /tmp/tir-byval-external-lit.o
; RUN: cc /tmp/tir-byval-lit.o /tmp/tir-byval-external-lit.o %S/Inputs/byval-harness.c -o /tmp/tir-byval-lit
; RUN: /tmp/tir-byval-lit

%S = type { i64, i64, i64, i64 }
%Small = type { i64 }
%Pair = type { i64, i64 }

define i64 @mutate_small(ptr byval(%Small) align 8 %s, i64 %later) {
  store i64 99, ptr %s, align 8
  %value = load i64, ptr %s, align 8
  %result = add i64 %value, %later
  ret i64 %result
}

define i64 @call_mutate_small(ptr %s, i64 %later) {
  %result = call i64 @mutate_small(ptr byval(%Small) align 8 %s, i64 %later)
  ret i64 %result
}

declare i64 @clang_read_small(ptr byval(%Small) align 8, i64)

define i64 @call_clang_read_small(ptr %s, i64 %later) {
  %result = call i64 @clang_read_small(ptr byval(%Small) align 8 %s, i64 %later)
  ret i64 %result
}

define i64 @mutate_pair(ptr byval(%Pair) align 8 %s, i64 %later) {
  %second = getelementptr %Pair, ptr %s, i32 0, i32 1
  store i64 77, ptr %second, align 8
  %value = load i64, ptr %second, align 8
  %result = add i64 %value, %later
  ret i64 %result
}

define i64 @call_mutate_pair(ptr %s, i64 %later) {
  %result = call i64 @mutate_pair(ptr byval(%Pair) align 8 %s, i64 %later)
  ret i64 %result
}

define i64 @sum_byval(ptr byval(%S) align 8 %s) {
  %b_ptr = getelementptr %S, ptr %s, i32 0, i32 1
  store i64 88, ptr %b_ptr, align 8
  %a_ptr = getelementptr %S, ptr %s, i32 0, i32 0
  %d_ptr = getelementptr %S, ptr %s, i32 0, i32 3
  %a = load i64, ptr %a_ptr, align 8
  %d = load i64, ptr %d_ptr, align 8
  %result = add i64 %a, %d
  ret i64 %result
}


define i64 @call_sum_byval(ptr %s) {
  %result = call i64 @sum_byval(ptr byval(%S) align 8 %s)
  ret i64 %result
}
