# RUN: tir mc --march=x86_64 --mattr=+bmi --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --mattr=+bmi --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: not tir mc --march=x86_64 --mattr=-bmi --filetype=obj-ascii %s
# RUN: not tir mc --march=x86_64 --mattr=-x86_64 --filetype=obj-ascii %s

	.global bmi_ops
bmi_ops:
	andn rax, rbx, rcx
	andn r8, r9, r10
	andn eax, ebx, ecx
	bextr rax, rbx, rcx
	bextr eax, ebx, ecx
	blsi rax, rbx
	blsi eax, ebx
	blsmsk rcx, rdx
	blsmsk ecx, edx
	blsr rdx, rsi
	blsr edx, esi
	tzcnt rax, rbx
	tzcnt eax, ebx

# CHECK:      asm.symbol {name = "bmi_ops"}
# CHECK-NEXT: x86_64.andn {dst = rax, src1 = rbx, src2 = rcx}
# CHECK-NEXT: x86_64.andn {dst = r8, src1 = r9, src2 = r10}
# CHECK-NEXT: x86_64.andn32 {dst = eax, src1 = ebx, src2 = ecx}
# CHECK-NEXT: x86_64.bextr {dst = rax, src = rbx, ctl = rcx}
# CHECK-NEXT: x86_64.bextr32 {dst = eax, src = ebx, ctl = ecx}
# CHECK-NEXT: x86_64.blsi {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.blsi32 {dst = eax, src = ebx}
# CHECK-NEXT: x86_64.blsmsk {dst = rcx, src = rdx}
# CHECK-NEXT: x86_64.blsmsk32 {dst = ecx, src = edx}
# CHECK-NEXT: x86_64.blsr {dst = rdx, src = rsi}
# CHECK-NEXT: x86_64.blsr32 {dst = edx, src = esi}
# CHECK-NEXT: x86_64.tzcnt {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.tzcnt32 {dst = eax, src = ebx}

# ENC:      [0xC4, 0xE2, 0xE0, 0xF2, 0xC1]
# ENC:      [0xC4, 0x42, 0xB0, 0xF2, 0xC2]
# ENC:      [0xC4, 0xE2, 0x60, 0xF2, 0xC1]
# ENC:      [0xC4, 0xE2, 0xF0, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0x70, 0xF7, 0xC3]
# ENC:      [0xC4, 0xE2, 0xF8, 0xF3, 0xDB]
# ENC:      [0xC4, 0xE2, 0x78, 0xF3, 0xDB]
# ENC:      [0xC4, 0xE2, 0xF0, 0xF3, 0xD2]
# ENC:      [0xC4, 0xE2, 0x70, 0xF3, 0xD2]
# ENC:      [0xC4, 0xE2, 0xE8, 0xF3, 0xCE]
# ENC:      [0xC4, 0xE2, 0x68, 0xF3, 0xCE]
# ENC:      [0xF3, 0x48, 0x0F, 0xBC, 0xC3]
# ENC:      [0xF3, 0x0F, 0xBC, 0xC3]
