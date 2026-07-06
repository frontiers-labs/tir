# RUN: isasim --march=riscv64 --entry=_start --until-pc=done --trace-instructions %s | filecheck %s

# Parser/decoder coverage for the A extension, FENCE, and FENCE.I: every
# instruction round-trips through the assembler into the machine dialect and the
# trace prints the decoded operands. The `done` sentinel is declared first so it
# lands after `_start`, giving the body a fall-through to stop on. All four
# ordering suffixes of one AMO exercise the aq/rl decode.
.global done
done:
  add x0, x0, x0
.global _start
_start:
  lr.w          t0, (a0)
  sc.w          t1, t2, (a0)
  amoadd.w      s0, s1, (a0)
  amoadd.w.aq   s0, s1, (a0)
  amoadd.w.rl   s0, s1, (a0)
  amoadd.w.aqrl s0, s1, (a0)
  amomaxu.w     s2, s3, (a0)
  lr.d          t0, (a0)
  sc.d          t1, t2, (a0)
  amoadd.d      s0, s1, (a0)
  fence 3, 3
  fence.i

# CHECK: lr.w rd=GPR[5], rs1=GPR[10]
# CHECK-NEXT: sc.w rd=GPR[6], rs2=GPR[7], rs1=GPR[10]
# CHECK-NEXT: amoadd.w rd=GPR[8], rs2=GPR[9], rs1=GPR[10]
# CHECK-NEXT: amoadd.w.aq rd=GPR[8], rs2=GPR[9], rs1=GPR[10]
# CHECK-NEXT: amoadd.w.rl rd=GPR[8], rs2=GPR[9], rs1=GPR[10]
# CHECK-NEXT: amoadd.w.aqrl rd=GPR[8], rs2=GPR[9], rs1=GPR[10]
# CHECK-NEXT: amomaxu.w rd=GPR[18], rs2=GPR[19], rs1=GPR[10]
# CHECK-NEXT: lr.d rd=GPR[5], rs1=GPR[10]
# CHECK-NEXT: sc.d rd=GPR[6], rs2=GPR[7], rs1=GPR[10]
# CHECK-NEXT: amoadd.d rd=GPR[8], rs2=GPR[9], rs1=GPR[10]
# CHECK-NEXT: fence pred=3, succ=3
# CHECK-NEXT: fence.i
