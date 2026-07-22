# RUN: tir mc --march=rv64ifd --filetype=obj-ascii %s | filecheck %s
# RUN: tir mc --march=rv32ifd --filetype=obj-ascii %s | filecheck %s

.global convert
convert:
    fcvt.d.w fa0, a0

# CHECK: convert:
# CHECK-NEXT: [0x53, 0x75, 0x05, 0xD2]
