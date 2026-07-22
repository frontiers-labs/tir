# RUN: tir mc --march=rv64ifd --filetype=obj-ascii %s | filecheck %s

.global bitcasts
bitcasts:
    fmv.d.x fa4, a4
    fmv.x.d a5, fa5

# CHECK: bitcasts:
# CHECK-NEXT: [0x53, 0x07, 0x07, 0xF2]
# CHECK-NEXT: [0xD3, 0x87, 0x07, 0xE2]
