# c.addi4spn scales its immediate by four: the encoding carries only imm[9:2],
# so an offset the operand's #[align(4)] does not admit has no encoding at all.
# The parser must reject it rather than let it round down to a different
# instruction.
# RUN: not tir mc --march=rv64ic %s

.global f
f:
    c.addi4spn a0, sp, 6
