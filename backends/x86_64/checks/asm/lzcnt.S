# RUN: tir mc --march=x86_64 --mattr=+lzcnt --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --mattr=+lzcnt --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC
# RUN: not tir mc --march=x86_64 --mattr=-lzcnt --filetype=obj-ascii %s
# RUN: not tir mc --march=x86_64 --mattr=-x86_64 --filetype=obj-ascii %s

	.global lzcnt_ops
lzcnt_ops:
	lzcnt rax, rbx
	lzcnt eax, ebx

# CHECK:      asm.symbol {name = "lzcnt_ops"}
# CHECK-NEXT: x86_64.lzcnt {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.lzcnt32 {dst = eax, src = ebx}

# ENC:      [0xF3, 0x48, 0x0F, 0xBD, 0xC3]
# ENC:      [0xF3, 0x0F, 0xBD, 0xC3]
