# RUN: tir mc --march=rv64ifd %s | filecheck %s
# RUN: not tir mc --march=rv32ifd %s
# RUN: not tir mc --march=rv64if %s

# fmv.d.x moves a whole binary64 pattern through an integer register, so it
# needs both D and a 64-bit base ISA.

.global f
f:
    fmv.d.x fa0, a0

# CHECK: fmv.d.x f10, x10
