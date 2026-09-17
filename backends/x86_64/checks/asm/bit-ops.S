# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global bit_tests
bit_tests:
	bt rax, rbx
	bts rax, rbx
	btr rcx, rdx
	btc rcx, rdx

# CHECK:      asm.symbol {name = "bit_tests"}
# CHECK-NEXT: x86_64.bt {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.bts {dst = rax, src = rbx}
# CHECK-NEXT: x86_64.btr {dst = rcx, src = rdx}
# CHECK-NEXT: x86_64.btc {dst = rcx, src = rdx}

# ENC:      [0x48, 0x0F, 0xA3, 0xD8]
# ENC:      [0x48, 0x0F, 0xAB, 0xD8]
# ENC:      [0x48, 0x0F, 0xB3, 0xD1]
# ENC:      [0x48, 0x0F, 0xBB, 0xD1]
