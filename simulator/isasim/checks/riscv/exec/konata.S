# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --timing --machine=rv64-ooo --konata - %s | filecheck %s

# The Konata pipeline log labels every traced instruction with its PC and
# disassembly (L command) and walks it through dispatch (D), execute (X), and
# retire (R) in a single forward-moving cycle domain.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  addi x1, x0, 5
  addi x2, x0, 7
  add  x3, x1, x2

# CHECK: Kanata{{\t}}0004
# CHECK-NEXT: C={{\t}}0
# CHECK-NEXT: I{{\t}}0{{\t}}0{{\t}}0
# CHECK-NEXT: L{{\t}}0{{\t}}0{{\t}}0x80000000: addi x1, x0, 5
# CHECK-NEXT: S{{\t}}0{{\t}}0{{\t}}D
# CHECK-NEXT: S{{\t}}0{{\t}}0{{\t}}X
# CHECK: L{{\t}}1{{\t}}0{{\t}}0x80000004: addi x2, x0, 7
# CHECK: L{{\t}}2{{\t}}0{{\t}}0x80000008: add x3, x1, x2
# CHECK: R{{\t}}2{{\t}}2{{\t}}0
# CHECK: timing[OutOfOrderCore / btfn]: 3 instructions
