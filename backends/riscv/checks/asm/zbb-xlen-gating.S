# RV32 Zbb encodings that differ from their RV64 forms.
# RUN: tir mc --march=rv32i_zbb %s | filecheck %s

.global f
f:
    zext.h x10, x11
    rev8 x10, x11

# CHECK: zext.h x10, x11
# CHECK-NEXT: rev8 x10, x11
