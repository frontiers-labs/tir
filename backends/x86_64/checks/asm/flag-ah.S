# RUN: tir mc --march=x86_64 --stage=isel %s | filecheck %s
# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

	.global ah_flags
ah_flags:
	sahf
	lahf

# CHECK:      asm.symbol {name = "ah_flags"}
# CHECK-NEXT: x86_64.sahf
# CHECK-NEXT: x86_64.lahf

# ENC:      [0x9E]
# ENC:      [0x9F]
