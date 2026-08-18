# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --timing --machine=rv64-ooo --konata - %s | filecheck %s

# A load walks through a distinct memory stage in the Konata log: address
# generation (X) followed by the memory access (M) before it retires.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  lw   x1, 0(a0)
  add  x2, x1, x1

# CHECK: Kanata{{\t}}0004
# CHECK: L{{\t}}0{{\t}}0{{\t}}0x80000000: lw x1, 0(x10)
# CHECK: S{{\t}}0{{\t}}0{{\t}}X
# CHECK: S{{\t}}0{{\t}}0{{\t}}M
# CHECK: R{{\t}}0{{\t}}0{{\t}}0
