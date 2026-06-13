# RUN: tir mc --march=rv64iv --filetype=obj-ascii %s | filecheck %s --check-prefix=BYTES

# A small slice of the RVV vector-vector arithmetic (OPIVV) instructions. The
# encodings match the RISC-V Vector spec bit for bit, e.g. `vadd.vv v1, v2, v3`
# is 0x022180d7.
.global vtest
vtest:
    vadd.vv v1, v2, v3
    vsub.vv v4, v5, v6
    vmul.vv v7, v8, v9

# BYTES: vtest:
# BYTES-NEXT: [0xD7, 0x80, 0x21, 0x02]
# BYTES-NEXT: [0x57, 0x02, 0x53, 0x0A]
# BYTES-NEXT: [0xD7, 0x83, 0x84, 0x96]
