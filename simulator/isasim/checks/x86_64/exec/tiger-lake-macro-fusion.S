# RUN: isasim --march=x86_64 --entry=_start --until-pc=0x8000000d --timing --machine=tiger-lake --konata - %s | filecheck %s

# The taken compare/branch pair executes as one timing unit. The trace count
# and Konata labels still keep both architectural instructions.
.global _start
_start:
  cmp rax, rax
  je 0
.global exit
exit:
  add rax, rax

# CHECK: L{{\t}}0{{\t}}0{{\t}}0x{{[0-9a-f]+}}: cmp rax, rax
# CHECK: L{{\t}}1{{\t}}0{{\t}}0x{{[0-9a-f]+}}: je 0
# CHECK: timing[{{.*}}]: 2 instructions,
