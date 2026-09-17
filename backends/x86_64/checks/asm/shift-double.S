# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global double_shifts
double_shifts:
	shld rax, rbx, 4
	shrd rax, rbx, 4
	shld rcx, rdx, cl
	shrd rcx, rdx, cl

# CHECK:      asm.symbol {name = "double_shifts"}
# CHECK-NEXT: x86_64.shld_imm {dst = rax, src = rbx, imm = 4}
# CHECK-NEXT: x86_64.shrd_imm {dst = rax, src = rbx, imm = 4}
# CHECK-NEXT: x86_64.shld_cl {dst = rcx, src = rdx}
# CHECK-NEXT: x86_64.shrd_cl {dst = rcx, src = rdx}

# ENC:      [0x48, 0x0F, 0xA4, 0xD8, 0x04]
# ENC:      [0x48, 0x0F, 0xAC, 0xD8, 0x04]
# ENC:      [0x48, 0x0F, 0xA5, 0xD1]
# ENC:      [0x48, 0x0F, 0xAD, 0xD1]
