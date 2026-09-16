# RUN: isasim --march=rv64imfd --entry=_start --until-pc=done --timing --machine=spacemit-x100 %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  fadd.s f10, f20, f21
  fadd.s f11, f20, f21
  fadd.s f12, f20, f21
  fadd.s f13, f20, f21

# CHECK: 4 instructions
# CHECK-SAME: 5 cycles
