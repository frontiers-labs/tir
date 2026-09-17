# Zbb golden encodings per the Zbb specification.
# RUN: tir mc --march=rv64i_zbb --filetype=obj-ascii %s | filecheck %s

.global f
f:
    andn x10, x11, x12
    orn x10, x11, x12
    xnor x10, x11, x12
    min x10, x11, x12
    minu x10, x11, x12
    max x10, x11, x12
    maxu x10, x11, x12
    rol x10, x11, x12
    ror x10, x11, x12
    clz x10, x11
    ctz x10, x11
    cpop x10, x11
    sext.b x10, x11
    sext.h x10, x11
    orc.b x10, x11
    zext.h x10, x11
    rev8 x10, x11

# CHECK: f:
# andn x10, x11, x12 -> 0x40C5F533
# CHECK-NEXT: [0x33, 0xF5, 0xC5, 0x40]
# orn x10, x11, x12 -> 0x40C5E533
# CHECK-NEXT: [0x33, 0xE5, 0xC5, 0x40]
# xnor x10, x11, x12 -> 0x40C5C533
# CHECK-NEXT: [0x33, 0xC5, 0xC5, 0x40]
# min x10, x11, x12 -> 0x0AC5C533
# CHECK-NEXT: [0x33, 0xC5, 0xC5, 0x0A]
# minu x10, x11, x12 -> 0x0AC5D533
# CHECK-NEXT: [0x33, 0xD5, 0xC5, 0x0A]
# max x10, x11, x12 -> 0x0AC5E533
# CHECK-NEXT: [0x33, 0xE5, 0xC5, 0x0A]
# maxu x10, x11, x12 -> 0x0AC5F533
# CHECK-NEXT: [0x33, 0xF5, 0xC5, 0x0A]
# rol x10, x11, x12 -> 0x60C59533
# CHECK-NEXT: [0x33, 0x95, 0xC5, 0x60]
# ror x10, x11, x12 -> 0x60C5D533
# CHECK-NEXT: [0x33, 0xD5, 0xC5, 0x60]
# clz x10, x11 -> 0x60059513
# CHECK-NEXT: [0x13, 0x95, 0x05, 0x60]
# ctz x10, x11 -> 0x60159513
# CHECK-NEXT: [0x13, 0x95, 0x15, 0x60]
# cpop x10, x11 -> 0x60259513
# CHECK-NEXT: [0x13, 0x95, 0x25, 0x60]
# sext.b x10, x11 -> 0x60459513
# CHECK-NEXT: [0x13, 0x95, 0x45, 0x60]
# sext.h x10, x11 -> 0x60559513
# CHECK-NEXT: [0x13, 0x95, 0x55, 0x60]
# orc.b x10, x11 -> 0x2875D513
# CHECK-NEXT: [0x13, 0xD5, 0x75, 0x28]
# zext.h x10, x11 -> 0x0805C53B
# CHECK-NEXT: [0x3B, 0xC5, 0x05, 0x08]
# rev8 x10, x11 -> 0x6B85D513
# CHECK-NEXT: [0x13, 0xD5, 0x85, 0x6B]
