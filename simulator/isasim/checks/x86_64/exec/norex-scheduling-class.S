# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --timing --machine=tiger-lake %s | filecheck %s

# An instruction's scheduling class is a field of its own record, indexed by
# machine, so no other opcode's class can stand in for it. `mov ax, imm16`
# selects `mov_imm16_norex`, whose mnemonic is the plain `mov` that the
# register-to-register move also carries. Looked up by mnemonic it silently got
# the rr form's ordinary single-cycle class; looked up by identity it is Tiger
# Lake's length-changing-prefix class, which holds the `lcp` decoder for three
# cycles per instruction.

.global done
done:
  add rax, rax
.global _start
_start:
  mov ax, 1
  mov bx, 2
  mov cx, 3
  mov dx, 4

# CHECK: 4 instructions, 11 cycles
