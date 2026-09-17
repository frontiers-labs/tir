# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global compare_exchange
compare_exchange:
	cmpxchg rax, rbx
	cmpxchg rax, r9
	cmpxchg r8, rbx

# CHECK:      asm.symbol {name = "compare_exchange"}
# CHECK-NEXT: x86_64.cmpxchg {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.cmpxchg {dst = rax, src = r9}
# CHECK-NEXT: x86_64.cmpxchg {dst = r8, src = rbx}

# ENC:      [0x48, 0x0F, 0xB1, 0xD8]
# ENC:      [0x4C, 0x0F, 0xB1, 0xC8]
# ENC:      [0x49, 0x0F, 0xB1, 0xD8]
