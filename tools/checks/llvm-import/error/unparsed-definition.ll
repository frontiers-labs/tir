; RUN: not tir llvm-import %s

define unsupported_signature i32 @must_not_disappear() {
  ret i32 1
}
