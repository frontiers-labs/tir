# RUN: tir mc --march=rv64ifd --filetype=obj-ascii %s | filecheck %s
# RUN: tir mc --march=rv32ifd --filetype=obj-ascii %s | filecheck %s

.global convert
convert:
    fcvt.d.w fa0, a0
    fcvt.d.wu fa1, a1
    fcvt.w.d a2, fa2
    fcvt.wu.d a3, fa3
    flt.d a0, fa0, fa1
    fle.d a0, fa0, fa1
    feq.d a0, fa0, fa1

# CHECK: convert:
# CHECK-NEXT: [0x53, 0x75, 0x05, 0xD2]
# CHECK-NEXT: [0xD3, 0xF5, 0x15, 0xD2]
# CHECK-NEXT: [0x53, 0x16, 0x06, 0xC2]
# CHECK-NEXT: [0xD3, 0x96, 0x16, 0xC2]
# CHECK-NEXT: [0x53, 0x15, 0xB5, 0xA2]
# CHECK-NEXT: [0x53, 0x05, 0xB5, 0xA2]
# CHECK-NEXT: [0x53, 0x25, 0xB5, 0xA2]
