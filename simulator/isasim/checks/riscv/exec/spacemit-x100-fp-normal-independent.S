# RUN: isasim --march=rv64imfd --entry=_start --until-pc=done --timing --machine=spacemit-x100 %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x20, x0, 1
  addi x21, x0, 2
  fcvt.d.w f20, x20
  fcvt.d.w f21, x21
  fdiv.d f10, f20, f21, rne
  fdiv.d f11, f20, f21, rne
  fdiv.d f12, f20, f21, rne
  fdiv.d f13, f20, f21, rne

# CHECK: 8 instructions, 79 cycles
