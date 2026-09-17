# Zbb RV32 encodings that differ from their RV64 forms.
# RUN: tir mc --march=rv32i_zbb --filetype=obj-ascii %s | filecheck %s

.global f
f:
    zext.h x10, x11
    rev8 x10, x11

# CHECK: f:
# zext.h x10, x11 -> 0x0805C533
# CHECK-NEXT: [0x33, 0xC5, 0x05, 0x08]
# rev8 x10, x11 -> 0x6985D513
# CHECK-NEXT: [0x13, 0xD5, 0x85, 0x69]
