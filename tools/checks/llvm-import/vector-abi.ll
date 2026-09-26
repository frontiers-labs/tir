; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-vector-abi-lit.o
; RUN: cc /tmp/tir-vector-abi-lit.o %S/Inputs/vector-abi-harness.c -o /tmp/tir-vector-abi-lit
; RUN: /tmp/tir-vector-abi-lit

define void @store_ninth(ptr %out, <2 x i64> %a0, <2 x i64> %a1, <2 x i64> %a2, <2 x i64> %a3, <2 x i64> %a4, <2 x i64> %a5, <2 x i64> %a6, <2 x i64> %a7, <2 x i64> %a8) {
  store <2 x i64> %a8, ptr %out, align 16
  ret void
}

define void @call_store_ninth(ptr %out, ptr %source) {
  %value = load <2 x i64>, ptr %source, align 16
  call void @store_ninth(ptr %out, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value, <2 x i64> %value)
  ret void
}

define void @store_tenth(ptr %out, <2 x i64> %a0, <2 x i64> %a1, <2 x i64> %a2, <2 x i64> %a3, <2 x i64> %a4, <2 x i64> %a5, <2 x i64> %a6, <2 x i64> %a7, <2 x i64> %a8, <2 x i64> %a9, i64 %later) {
  store <2 x i64> %a8, ptr %out, align 16
  %second = getelementptr i8, ptr %out, i64 16
  store <2 x i64> %a9, ptr %second, align 16
  %scalar = getelementptr i8, ptr %out, i64 32
  store i64 %later, ptr %scalar, align 8
  ret void
}

define void @call_store_tenth(ptr %out, ptr %first, ptr %second, i64 %later) {
  %first_value = load <2 x i64>, ptr %first, align 16
  %second_value = load <2 x i64>, ptr %second, align 16
  call void @store_tenth(ptr %out, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %first_value, <2 x i64> %second_value, i64 %later)
  ret void
}

define <2 x i64> @identity_vector(<2 x i64> %value) {
  ret <2 x i64> %value
}

define void @call_identity_vector(ptr %out, ptr %source) {
  %value = load <2 x i64>, ptr %source, align 16
  %result = call <2 x i64> @identity_vector(<2 x i64> %value)
  store <2 x i64> %result, ptr %out, align 16
  ret void
}

define void @mixed_vector(ptr %out, double %first, <2 x i64> %value, double %last) {
  store <2 x i64> %value, ptr %out, align 16
  %sum = fadd double %first, %last
  %tail = getelementptr i8, ptr %out, i64 16
  store double %sum, ptr %tail, align 8
  ret void
}

define void @call_mixed_vector(ptr %out, ptr %source, double %first, double %last) {
  %value = load <2 x i64>, ptr %source, align 16
  %returned = call <2 x i64> @identity_vector(<2 x i64> %value)
  call void @mixed_vector(ptr %out, double %first, <2 x i64> %returned, double %last)
  ret void
}
