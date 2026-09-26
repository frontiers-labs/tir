; RUN: tir mc --march=riscv64 --stage=isel %s llvm | filecheck %s --check-prefix=RV64
; RUN: tir mc --march=arm64 --stage=isel %s llvm | filecheck %s --check-prefix=ARM64
; RUN: tir mc --march=x86_64 --stage=isel %s llvm | filecheck %s --check-prefix=X86

; Comparisons, right shifts, and divisions read every operand bit. A target
; without the narrow form computes them on extended operands; one with it keeps
; its own instruction.

define i32 @eq8(i8 %a, i8 %b) {
  %c = icmp eq i8 %a, %b
  %z = zext i1 %c to i32
  ret i32 %z
}

define i8 @shr8(i8 %a, i8 %b) {
  %c = lshr i8 %a, %b
  ret i8 %c
}

define i8 @udiv8(i8 %a, i8 %b) {
  %c = udiv i8 %a, %b
  ret i8 %c
}

define i32 @choose(i32 %a, i32 %b, i32 %if_true, i32 %if_false) {
  %c = icmp slt i32 %a, %b
  %r = select i1 %c, i32 %if_true, i32 %if_false
  ret i32 %r
}

; RV64-LABEL: asm.symbol {name = "eq8", arg_regs = [%[[A:[0-9]+]]:GPR, %[[B:[0-9]+]]:GPR]}
; RV64-NEXT: riscv.andi {rd = %[[AX:[0-9]+]]:GPR, rs1 = %[[A]]:GPR, imm = 255}
; RV64-NEXT: riscv.andi {rd = %[[BX:[0-9]+]]:GPR, rs1 = %[[B]]:GPR, imm = 255}
; RV64-NEXT: riscv.xor {rd = %[[D:[0-9]+]]:GPR, rs1 = %[[AX]]:GPR, rs2 = %[[BX]]:GPR}
; RV64-NEXT: riscv.sltiu {rd = %{{[0-9]+}}:GPR, rs1 = %[[D]]:GPR, imm = 1}
; RV64-LABEL: asm.symbol {name = "shr8"
; RV64: riscv.srlw
; RV64-LABEL: asm.symbol {name = "udiv8", arg_regs = [%[[P:[0-9]+]]:GPR, %[[Q:[0-9]+]]:GPR]}
; RV64-NEXT: riscv.andi {rd = %[[PX:[0-9]+]]:GPR, rs1 = %[[P]]:GPR, imm = 255}
; RV64-NEXT: riscv.andi {rd = %[[QX:[0-9]+]]:GPR, rs1 = %[[Q]]:GPR, imm = 255}
; RV64-NEXT: riscv.divuw {rd = %{{[0-9]+}}:GPR, rs1 = %[[PX]]:GPR, rs2 = %[[QX]]:GPR}

; ARM64-LABEL: asm.symbol {name = "eq8", arg_regs = [%[[A:[0-9]+]]:GPR, %[[B:[0-9]+]]:GPR]}
; ARM64: arm64.movz {rd = %[[M:[0-9]+]]:GPR, imm = 255}
; ARM64-NEXT: arm64.and {rd = %[[BX:[0-9]+]]:GPR, rn = %[[B]]:GPR, rm = %[[M]]:GPR}
; ARM64: arm64.lslv {rd = %[[AH:[0-9]+]]:GPR, rn = %[[A]]:GPR
; ARM64-NEXT: arm64.eor_lsr {rd = %[[D:[0-9]+]]:GPR, rn = %[[BX]]:GPR, rm = %[[AH]]:GPR, imm = 56}
; ARM64-NEXT: arm64.cmp_imm {rn = %[[D]]:GPR, imm = 1}
; ARM64-LABEL: asm.symbol {name = "shr8"
; ARM64: arm64.lsrv_w
; ARM64-LABEL: asm.symbol {name = "udiv8"
; ARM64: arm64.udiv_word
; ARM64-LABEL: asm.symbol {name = "choose"
; ARM64-NEXT: arm64.cmp_w
; ARM64-NEXT: arm64.csel_lt

; X86-LABEL: asm.symbol {name = "eq8"
; X86-NEXT: x86_64.cmp8
; X86-LABEL: asm.symbol {name = "shr8"
; X86-NEXT: x86_64.shr_cl8
; X86-LABEL: asm.symbol {name = "udiv8", arg_regs = [%[[P:[0-9]+]]:GPR, %[[Q:[0-9]+]]:GPR]}
; X86-NEXT: x86_64.and_imm32 {dst = %[[PX:[0-9]+]]:GPR32, dst_tied = %[[P]]:GPR, imm = 255}
; X86-NEXT: x86_64.and_imm32 {dst = %[[QX:[0-9]+]]:GPR32, dst_tied = %[[Q]]:GPR, imm = 255}
; X86: x86_64.unsigned_divide32 {dst = %[[QX]]:GPR32, eax = %[[PX]]:GPR32
; X86-LABEL: asm.symbol {name = "choose"
; X86-NEXT: x86_64.cmp32
; X86-NEXT: x86_64.cmovl
