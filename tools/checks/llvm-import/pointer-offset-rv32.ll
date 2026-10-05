; RUN: tir mc --march=rv32im --stage=isel %s | filecheck %s

; Import must use the selected target before the pointer verifier runs. The
; pointer array and GEP scale must also use four-byte pointers on RV32.
@slots = global [2 x ptr] zeroinitializer, align 4
@symbols = global [2 x ptr] [ptr @slots, ptr @slots], align 4

; CHECK: global @slots size 8 align 4
; CHECK: global @symbols align 4 bytes [0, 0, 0, 0, 0, 0, 0, 0]
; CHECK-SAME: offset = 4
; CHECK-SAME: width = 4
; CHECK-LABEL: asm.symbol {name = "offset_and_select"
; CHECK: riscv.slli {rd = %[[SHIFTED:[0-9]+]]:GPR, rs1 = %{{[0-9]+}}:GPR, imm = 16}
; CHECK: riscv.srai {rd = %[[SIGNED:[0-9]+]]:GPR, rs1 = %[[SHIFTED]]:GPR, imm = 16}
; CHECK: riscv.slli {rd = %{{[0-9]+}}:GPR, rs1 = %[[SIGNED]]:GPR, imm = 2}
define ptr @offset_and_select(ptr %base, i16 %index, i1 %condition) {
  %element = getelementptr ptr, ptr %base, i16 %index
  %address = ptrtoint ptr %element to i32
  %roundtrip = inttoptr i32 %address to ptr
  %result = select i1 %condition, ptr %roundtrip, ptr %base
  ret ptr %result
}
