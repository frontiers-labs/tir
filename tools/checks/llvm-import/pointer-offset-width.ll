; RUN: tir llvm-import %s | tir opt --verify | filecheck %s

; Pointer offsets are pointer-sized, so every width change LLVM leaves implicit
; is an op of its own: a getelementptr index is sign-extended, and inttoptr and
; ptrtoint zero-extend or truncate between the integer and the address.

; CHECK-LABEL: func.func @narrow_index
; CHECK-NEXT: %[[INDEX:[0-9]+]] = extsi %{{[0-9]+}} : !i64
; CHECK-NEXT: ptr.ptradd %{{[0-9]+}}, %[[INDEX]] : !ptr.p
define ptr @narrow_index(ptr %p, i32 %i) {
  %q = getelementptr i8, ptr %p, i32 %i
  ret ptr %q
}

; CHECK-LABEL: func.func @wide_index
; CHECK-NEXT: %[[INDEX:[0-9]+]] = trunci %{{[0-9]+}} : !i64
; CHECK-NEXT: ptr.ptradd %{{[0-9]+}}, %[[INDEX]] : !ptr.p
define ptr @wide_index(ptr %p, i128 %i) {
  %q = getelementptr i8, ptr %p, i128 %i
  ret ptr %q
}

; CHECK-LABEL: func.func @narrow_inttoptr
; CHECK-NEXT: %[[ADDRESS:[0-9]+]] = extui %{{[0-9]+}} : !i64
; CHECK-NEXT: %[[NULL:[0-9]+]] = ptr.null : !ptr.p
; CHECK-NEXT: ptr.ptradd %[[NULL]], %[[ADDRESS]] : !ptr.p
define ptr @narrow_inttoptr(i32 %x) {
  %p = inttoptr i32 %x to ptr
  ret ptr %p
}

; CHECK-LABEL: func.func @narrow_ptrtoint
; CHECK-NEXT: %[[NULL:[0-9]+]] = ptr.null : !ptr.p
; CHECK-NEXT: %[[ADDRESS:[0-9]+]] = ptr.ptrdiff %{{[0-9]+}}, %[[NULL]] : !i64
; CHECK-NEXT: trunci %[[ADDRESS]] : !i32
define i32 @narrow_ptrtoint(ptr %p) {
  %x = ptrtoint ptr %p to i32
  ret i32 %x
}
