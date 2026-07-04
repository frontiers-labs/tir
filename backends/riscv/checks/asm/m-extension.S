# RUN: tir mc --march=rv64im --filetype=obj-ascii %s | filecheck %s
# RUN: tir mc --march=rv64i --mattr=+m --filetype=obj-ascii %s | filecheck %s
# The *W forms are rv64-only; assembling them for rv32 must fail.
# RUN: not tir mc --march=rv32im %s

# Full RV32M/RV64M register-register set with known-good encodings.
.global f
f:
    mul    x1, x2, x3
    mulh   x4, x5, x6
    mulhsu x7, x8, x9
    mulhu  x10, x11, x12
    div    x13, x14, x15
    divu   x16, x17, x18
    rem    x19, x20, x21
    remu   x22, x23, x24
    mulw   x1, x2, x3
    divw   x4, x5, x6
    divuw  x7, x8, x9
    remw   x10, x11, x12
    remuw  x13, x14, x15

# CHECK: [0xB3, 0x00, 0x31, 0x02]
# CHECK: [0x33, 0x92, 0x62, 0x02]
# CHECK: [0xB3, 0x23, 0x94, 0x02]
# CHECK: [0x33, 0xB5, 0xC5, 0x02]
# CHECK: [0xB3, 0x46, 0xF7, 0x02]
# CHECK: [0x33, 0xD8, 0x28, 0x03]
# CHECK: [0xB3, 0x69, 0x5A, 0x03]
# CHECK: [0x33, 0xFB, 0x8B, 0x03]
# CHECK: [0xBB, 0x00, 0x31, 0x02]
# CHECK: [0x3B, 0xC2, 0x62, 0x02]
# CHECK: [0xBB, 0x53, 0x94, 0x02]
# CHECK: [0x3B, 0xE5, 0xC5, 0x02]
# CHECK: [0xBB, 0x76, 0xF7, 0x02]
