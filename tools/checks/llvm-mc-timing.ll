; RUN: env TIR_TIME_PASSES=1 tir mc --march=x86_64 --filetype=obj-ascii %s 2>&1 | filecheck %s
; RUN: env TIR_TIME_PASSES=0 tir mc --march=x86_64 --filetype=obj-ascii %s 2>&1 | filecheck %s --check-prefix=DISABLED

; CHECK: tir-time: import_ms={{[0-9.]+}} backend_ms={{[0-9.]+}}
; DISABLED-NOT: tir-time:

define i32 @answer() {
  ret i32 42
}
