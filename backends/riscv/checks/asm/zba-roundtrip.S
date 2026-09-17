# Zba roundtrip and golden encodings per the Zba specification.
# RUN: tir mc --march=rv64i_zba %s | filecheck %s
# RUN: tir mc --march=rv64i_zba --filetype=obj-ascii %s | filecheck --check-prefix=ENC %s

.global f
f:
    sh1add x10, x11, x12
    sh2add x10, x11, x12
    sh3add x10, x11, x12

    add.uw x10, x11, x12
    sh1add.uw x10, x11, x12
    sh2add.uw x10, x11, x12
    sh3add.uw x10, x11, x12
    slli.uw x10, x11, 0
    slli.uw x10, x11, 31
    slli.uw x10, x11, 32
    slli.uw x10, x11, 63

# CHECK: sh1add x10, x11, x12
# CHECK-NEXT: sh2add x10, x11, x12
# CHECK-NEXT: sh3add x10, x11, x12
# CHECK-NEXT: add.uw x10, x11, x12
# CHECK-NEXT: sh1add.uw x10, x11, x12
# CHECK-NEXT: sh2add.uw x10, x11, x12
# CHECK-NEXT: sh3add.uw x10, x11, x12
# CHECK-NEXT: slli.uw x10, x11, 0
# CHECK-NEXT: slli.uw x10, x11, 31
# CHECK-NEXT: slli.uw x10, x11, 32
# CHECK-NEXT: slli.uw x10, x11, 63
# ENC: [0x33, 0xA5, 0xC5, 0x20]
# ENC-NEXT: [0x33, 0xC5, 0xC5, 0x20]
# ENC-NEXT: [0x33, 0xE5, 0xC5, 0x20]
# ENC-NEXT: [0x3B, 0x85, 0xC5, 0x08]
# ENC-NEXT: [0x3B, 0xA5, 0xC5, 0x20]
# ENC-NEXT: [0x3B, 0xC5, 0xC5, 0x20]
# ENC-NEXT: [0x3B, 0xE5, 0xC5, 0x20]
# ENC-NEXT: [0x1B, 0x95, 0x05, 0x08]
# ENC-NEXT: [0x1B, 0x95, 0xF5, 0x09]
# ENC-NEXT: [0x1B, 0x95, 0x05, 0x0A]
# ENC-NEXT: [0x1B, 0x95, 0xF5, 0x0B]
