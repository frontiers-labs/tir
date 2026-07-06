# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

# Canonical REX-free and imm8 (0x83) encodings, matching GNU as. Reference bytes
# from GNU as 2.42 (`as --64 -msyntax=intel -mnaked-reg`, `objdump -d -M intel`):
#   add ecx,edx  -> 01 d1          add ecx,r8d -> 44 01 c1
#   add al,bl    -> 00 d8          add sil,dil -> 40 00 fe   (empty REX required)
#   add rcx,42   -> 48 83 c1 2a    add ecx,42  -> 83 c1 2a
#   add ecx,300  -> 81 c1 2c 01 00 00
#   push rbx     -> 53             push r12    -> 41 54
#   jmp rsi      -> ff e6          sete al     -> 0f 94 c0

	.global canon
canon:
	add ecx, edx
	add ecx, r8d
	add al, bl
	add sil, dil
	add rcx, 42
	add ecx, 42
	add ecx, 300
	push rbx
	push r12
	jmp *rsi
	sete al

# CHECK: canon:
# No REX for eax..edi; REX.R appears only once r8d sets its 4th bit.
# CHECK-NEXT: [0x01, 0xD1]
# CHECK-NEXT: [0x44, 0x01, 0xC1]
# al/bl need no REX; spl/bpl/sil/dil require an empty 0x40.
# CHECK-NEXT: [0x00, 0xD8]
# CHECK-NEXT: [0x40, 0x00, 0xFE]
# imm8 0x83 short form; REX.W stays for the 64-bit form.
# CHECK-NEXT: [0x48, 0x83, 0xC1, 0x2A]
# CHECK-NEXT: [0x83, 0xC1, 0x2A]
# 300 exceeds i8, so the imm32 0x81 form (still REX-free) is used.
# CHECK-NEXT: [0x81, 0xC1, 0x2C, 0x01, 0x00, 0x00]
# CHECK-NEXT: [0x53]
# CHECK-NEXT: [0x41, 0x54]
# CHECK-NEXT: [0xFF, 0xE6]
# CHECK-NEXT: [0x0F, 0x94, 0xC0]
