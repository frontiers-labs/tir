target triple = "x86_64-pc-linux-gnu"
%Small = type { i64 }

declare i64 @mutate_small(ptr byval(%Small) align 8, i64)

define i64 @clang_read_small(ptr byval(%Small) align 8 %s, i64 %later) {
  %value = load i64, ptr %s, align 8
  %result = add i64 %value, %later
  ret i64 %result
}

define i64 @clang_call_tir_small(ptr %s, i64 %later) {
  %result = call i64 @mutate_small(ptr byval(%Small) align 8 %s, i64 %later)
  ret i64 %result
}
