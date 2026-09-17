# Zba golden encodings per the Zba specification.
# RUN: tir mc --march=rv64i_zba --filetype=obj-ascii %s | filecheck %s

.global f
f:
    add.uw x10, x11, x12
    sh1add.uw x10, x11, x12
    sh2add.uw x10, x11, x12
    sh3add.uw x10, x11, x12
    slli.uw x10, x11, 31

# CHECK: f:
# add.uw x10, x11, x12 -> 0x08C5853B
# CHECK-NEXT: [0x3B, 0x85, 0xC5, 0x08]
# sh1add.uw x10, x11, x12 -> 0x20C5A53B
# CHECK-NEXT: [0x3B, 0xA5, 0xC5, 0x20]
# sh2add.uw x10, x11, x12 -> 0x20C5C53B
# CHECK-NEXT: [0x3B, 0xC5, 0xC5, 0x20]
# sh3add.uw x10, x11, x12 -> 0x20C5E53B
# CHECK-NEXT: [0x3B, 0xE5, 0xC5, 0x20]
# slli.uw x10, x11, 31 -> 0x09F5951B
# CHECK-NEXT: [0x1B, 0x95, 0xF5, 0x09]
