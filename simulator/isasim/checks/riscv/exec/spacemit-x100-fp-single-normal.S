# RUN: isasim --march=rv64imfd --entry=_start --until-pc=done --timing --machine=spacemit-x100 %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x20, x0, 1
  addi x21, x0, 2
  fcvt.s.w f20, x20
  fcvt.s.w f21, x21
  fdiv.s f10, f20, f21, rne
  fdiv.s f11, f20, f21, rne
  fdiv.s f12, f20, f21, rne
  fdiv.s f13, f20, f21, rne

# CHECK: 8 instructions, 47 cycles
