# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

.global legacy_norex
legacy_norex:
    mov eax, [rbx]
    mov [rbx], ecx
    mov [rbx], cx
    mov [rbx], cl
    movzx eax, bl
    movsx eax, cx
    movzx eax, byte ptr [rbx]
    movsx eax, word ptr [rbx]
    imul eax, ebx
    idiv ebx
    div ebx
    cdq

# CHECK: legacy_norex:
# CHECK-NEXT: [0x8B, 0x03]
# CHECK-NEXT: [0x89, 0x0B]
# CHECK-NEXT: [0x66, 0x89, 0x0B]
# CHECK-NEXT: [0x88, 0x0B]
# CHECK-NEXT: [0x0F, 0xB6, 0xC3]
# CHECK-NEXT: [0x0F, 0xBF, 0xC1]
# CHECK-NEXT: [0x0F, 0xB6, 0x03]
# CHECK-NEXT: [0x0F, 0xBF, 0x03]
# CHECK-NEXT: [0x0F, 0xAF, 0xC3]
# CHECK-NEXT: [0xF7, 0xFB]
# CHECK-NEXT: [0xF7, 0xF3]
# CHECK-NEXT: [0x99]
