# RUN: tir mc --march=x86_64 --mattr=+bmi2 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --mattr=+bmi2 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: not tir mc --march=x86_64 --mattr=-bmi2 --filetype=obj-ascii %s
# RUN: not tir mc --march=x86_64 --mattr=-x86_64 --filetype=obj-ascii %s

	.global bmi2_ops
bmi2_ops:
	bzhi rax, rbx, rcx
	bzhi eax, ebx, ecx
	mulx rax, rbx, rcx
	mulx eax, ebx, ecx
	pdep rax, rbx, rcx
	pdep eax, ebx, ecx
	pext rax, rbx, rcx
	pext eax, ebx, ecx
	rorx rax, rbx, 4
	rorx eax, ebx, 4
	sarx rax, rbx, rcx
	sarx eax, ebx, ecx
	shlx rax, rbx, rcx
	shlx eax, ebx, ecx
	shrx rax, rbx, rcx
	shrx eax, ebx, ecx

# CHECK:      asm.symbol {name = "bmi2_ops"}
# CHECK-NEXT: x86_64.bzhi {dst = rax, src = rbx, ctl = rcx}
# CHECK-NEXT: x86_64.bzhi32 {dst = eax, src = ebx, ctl = ecx}
# CHECK-NEXT: x86_64.mulx {hi = rax, lo = rbx, src = rcx}
# CHECK-NEXT: x86_64.mulx32 {hi = eax, lo = ebx, src = ecx}
# CHECK-NEXT: x86_64.pdep {dst = rax, src1 = rbx, src2 = rcx}
# CHECK-NEXT: x86_64.pdep32 {dst = eax, src1 = ebx, src2 = ecx}
# CHECK-NEXT: x86_64.pext {dst = rax, src1 = rbx, src2 = rcx}
# CHECK-NEXT: x86_64.pext32 {dst = eax, src1 = ebx, src2 = ecx}
# CHECK-NEXT: x86_64.rorx {dst = rax, src = rbx, imm = 4}
# CHECK-NEXT: x86_64.rorx32 {dst = eax, src = ebx, imm = 4}
# CHECK-NEXT: x86_64.sarx {dst = rax, src = rbx, ctl = rcx}
# CHECK-NEXT: x86_64.sarx32 {dst = eax, src = ebx, ctl = ecx}
# CHECK-NEXT: x86_64.shlx {dst = rax, src = rbx, ctl = rcx}
# CHECK-NEXT: x86_64.shlx32 {dst = eax, src = ebx, ctl = ecx}
# CHECK-NEXT: x86_64.shrx {dst = rax, src = rbx, ctl = rcx}
# CHECK-NEXT: x86_64.shrx32 {dst = eax, src = ebx, ctl = ecx}

# ENC:      [0xC4, 0xE2, 0xF0, 0xF5, 0xC3]
# ENC:      [0xC4, 0xE2, 0x70, 0xF5, 0xC3]
# ENC:      [0xC4, 0xE2, 0xE3, 0xF6, 0xC1]
# ENC:      [0xC4, 0xE2, 0x63, 0xF6, 0xC1]
# ENC:      [0xC4, 0xE2, 0xE3, 0xF5, 0xC1]
# ENC:      [0xC4, 0xE2, 0x63, 0xF5, 0xC1]
# ENC:      [0xC4, 0xE2, 0xE2, 0xF5, 0xC1]
# ENC:      [0xC4, 0xE2, 0x62, 0xF5, 0xC1]
# ENC:      [0xC4, 0xE3, 0xFB, 0xF0, 0xC3, 0x04]
# ENC:      [0xC4, 0xE3, 0x7B, 0xF0, 0xC3, 0x04]
# ENC:      [0xC4, 0xE2, 0xF2, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0x72, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0xF1, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0x71, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0xF3, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0x73, 0xF7, 0xC3]
