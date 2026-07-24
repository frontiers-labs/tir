# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Fixed-register division encodings (group-3 0xF7 /7 idiv, /6 div) and the
# dividend-extension definers. Reference bytes from GNU as 2.42
# (`as --64 -msyntax=intel -mnaked-reg`, `objdump -d -M intel`):
#   cqo          -> 48 99          idiv rbx -> 48 f7 fb
#   xor edx,edx  -> 31 d2          div rcx  -> 48 f7 f1
#   idiv r10     -> 49 f7 fa       cdq      -> 99

	.global division
division:
	cqo
	idiv rbx
	idiv r10
	xor edx, edx
	div rcx
	cdq
	idiv ebx
	div ecx

# CHECK: division:
# CHECK-NEXT: [0x48, 0x99]
# CHECK-NEXT: [0x48, 0xF7, 0xFB]
# CHECK-NEXT: [0x49, 0xF7, 0xFA]
# CHECK-NEXT: [0x31, 0xD2]
# CHECK-NEXT: [0x48, 0xF7, 0xF1]
# CHECK-NEXT: [0x99]
# The 32-bit low-register forms keep the empty 0x40 REX, matching the
# RegUnary32 template (as the reference div32 encodings did).
# CHECK-NEXT: [0x40, 0xF7, 0xFB]
# CHECK-NEXT: [0x40, 0xF7, 0xF1]
