# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global unary_counts
unary_counts:
	inc rax
	dec rcx
	inc r8

# CHECK:      asm.symbol {name = "unary_counts"}
# CHECK-NEXT: x86_64.inc {dst = rax}
# CHECK-NEXT: x86_64.dec {dst = rcx}
# CHECK-NEXT: x86_64.inc {dst = r8}

# ENC:      [0x48, 0xFF, 0xC0]
# ENC:      [0x48, 0xFF, 0xC9]
# ENC:      [0x49, 0xFF, 0xC0]
