; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

%struct.State = type { i32, [4 x i16], ptr }

@seed = internal global i32 7, align 4
@name = private unnamed_addr constant [5 x i8] c"test\00", align 1
@state = global %struct.State zeroinitializer, align 8

declare i32 @printf(ptr, ...)

define i32 @step(ptr %state, i32 %count, ptr %callback) {
entry:
  %slot = getelementptr inbounds %struct.State, ptr %state, i64 0, i32 1, i64 2
  %value = load i16, ptr %slot, align 2
  %wide = zext i16 %value to i32
  %is_zero = icmp eq i32 %count, 0
  %divisor = select i1 %is_zero, i32 1, i32 %count
  %quotient = udiv i32 %wide, %divisor
  %remainder = urem i32 %wide, %divisor
  %next = add nuw i32 %quotient, %remainder
  br label %loop

loop:
  %index = phi i32 [ 0, %entry ], [ %inc, %body ]
  %done = icmp uge i32 %index, %count
  br i1 %done, label %exit, label %body

body:
  %inc = add nuw i32 %index, 1
  br label %loop

exit:
  %format = getelementptr inbounds [5 x i8], ptr @name, i64 0, i64 0
  %printed = call i32 (ptr, ...) @printf(ptr %format, i32 %next)
  %called = call i32 %callback(i32 %printed)
  ret i32 %called
}

; CHECK: global private @seed align 4 bytes [7, 0, 0, 0]
; CHECK: global private @name align 1 section ".rodata" bytes [116, 101, 115, 116, 0]
; CHECK: global @state size 24 align 8
; CHECK: func @step
; CHECK: ptradd
; CHECK: divui
; CHECK: remui
; CHECK: call
