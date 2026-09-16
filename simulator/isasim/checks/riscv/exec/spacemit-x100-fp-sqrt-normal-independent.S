# RUN: isasim --march=rv64imfd --entry=_start --until-pc=done --timing --machine=spacemit-x100 %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x20, x0, 1
  fcvt.d.w f20, x20
  fsqrt.d f10, f20, rne
  fsqrt.d f11, f20, rne
  fsqrt.d f12, f20, rne
  fsqrt.d f13, f20, rne

# CHECK: 6 instructions, 78 cycles
