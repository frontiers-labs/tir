# RUN: tir mc --march=rv64i %s | filecheck %s --check-prefix=ASM
# RUN: tir mc --march=rv64i --filetype=obj-ascii %s | filecheck %s --check-prefix=BYTES

    .byte 1
    .byte -1
    .byte 0x7f
    .half 0x1234
    .word 0xdeadbeef
    .dword 0x1122334455667788
    .space 3

# ASM:      .byte 1
# ASM-NEXT: .byte -1
# ASM-NEXT: .byte 127
# ASM-NEXT: .half 4660
# ASM-NEXT: .word 3735928559
# ASM-NEXT: .dword 1234605616436508552
# ASM-NEXT: .space 3

# BYTES: .section .text
# BYTES-NEXT: [0x01, 0xFF, 0x7F, 0x34, 0x12, 0xEF, 0xBE, 0xAD, 0xDE, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x00]
