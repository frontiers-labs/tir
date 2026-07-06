# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

.global count
count:
    mov rax, 0
back:
    add rax, 1
    jne back
    jmp done
    mov rbx, 5
done:
    ret

# Variable-length x86 encodings; rel32 displacements are measured from the end of
# the branch instruction (jcc is 6 bytes, jmp rel32 is 5 bytes):
#   0x00  mov rax, 0   count       (7 bytes)
#   0x07  add rax, 1   back        (7 bytes)
#   0x0E  jne back  -> 0x07 - 0x14 = -13 = 0xFFFFFFF3
#   0x14  jmp done  -> 0x20 - 0x19 = +7  = 0x00000007
#   0x19  mov rbx, 5               (7 bytes)
#   0x20  ret        done
# CHECK: .section .text
# CHECK-NEXT: count:
# CHECK-NEXT: [0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x48, 0x83, 0xC0, 0x01]
# CHECK-NEXT: [0x0F, 0x85, 0xF6, 0xFF, 0xFF, 0xFF]
# CHECK-NEXT: [0xE9, 0x07, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x48, 0xC7, 0xC3, 0x05, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0xC3]
