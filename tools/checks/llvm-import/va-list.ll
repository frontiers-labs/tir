; RUN: tir llvm-import --march x86_64 %s | tir opt --verify | filecheck %s
; RUN: tir mc --march x86_64 --filetype obj %s llvm -o /tmp/tir-va-list-lit.o
; RUN: cc /tmp/tir-va-list-lit.o %S/Inputs/va-list-harness.c -o /tmp/tir-va-list-lit
; RUN: /tmp/tir-va-list-lit

%struct.__va_list_tag = type { i32, i32, ptr, ptr }

define void @touch(i32 %fixed, ...) {
  %list = alloca [1 x %struct.__va_list_tag], align 16
  %address = getelementptr [1 x %struct.__va_list_tag], ptr %list, i64 0, i64 0
  call void @llvm.va_start.p0(ptr %address)
  call void @llvm.va_end.p0(ptr %address)
  ret void
}

define i32 @first_int(i32 %fixed, ...) {
  %list = alloca %struct.__va_list_tag, align 16
  call void @llvm.va_start.p0(ptr %list)
  %save.addr = getelementptr %struct.__va_list_tag, ptr %list, i32 0, i32 3
  %save = load ptr, ptr %save.addr
  %arg.addr = getelementptr i8, ptr %save, i64 8
  %arg = load i32, ptr %arg.addr
  ret i32 %arg
}

define double @first_double(i32 %fixed, ...) {
  %list = alloca %struct.__va_list_tag, align 16
  call void @llvm.va_start.p0(ptr %list)
  %fp.addr = getelementptr %struct.__va_list_tag, ptr %list, i32 0, i32 1
  %fp = load i32, ptr %fp.addr
  %save.addr = getelementptr %struct.__va_list_tag, ptr %list, i32 0, i32 3
  %save = load ptr, ptr %save.addr
  %arg.addr = getelementptr i8, ptr %save, i64 48
  %arg = load double, ptr %arg.addr
  ret double %arg
}

define i32 @overflow_int(i32 %a, i32 %b, i32 %c, i32 %d, i32 %e, i32 %f, ...) {
  %list = alloca %struct.__va_list_tag, align 16
  call void @llvm.va_start.p0(ptr %list)
  %overflow.addr = getelementptr %struct.__va_list_tag, ptr %list, i32 0, i32 2
  %overflow = load ptr, ptr %overflow.addr
  %arg = load i32, ptr %overflow
  ret i32 %arg
}

define double @overflow_double(i32 %fixed, double %a, double %b, double %c, double %d, double %e, double %f, double %g, double %h, ...) {
  %list = alloca %struct.__va_list_tag, align 16
  call void @llvm.va_start.p0(ptr %list)
  %fp.addr = getelementptr %struct.__va_list_tag, ptr %list, i32 0, i32 1
  %fp = load i32, ptr %fp.addr
  %overflow.addr = getelementptr %struct.__va_list_tag, ptr %list, i32 0, i32 2
  %overflow = load ptr, ptr %overflow.addr
  %arg = load double, ptr %overflow
  ret double %arg
}

declare void @llvm.va_start.p0(ptr)
declare void @llvm.va_end.p0(ptr)

; CHECK: func.func @touch(%{{[0-9]+}}: !i32, {{.*}}, ...) implicit_arguments 14 entry_sp
; CHECK: ptr.alloca {size = 176, align = 16}
; CHECK-NOT: func.va_start
; CHECK-NOT: func.va_end
