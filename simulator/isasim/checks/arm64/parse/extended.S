# RUN: isasim --march=arm64 --entry=_start --until-pc=done --trace-instructions %s | filecheck %s

# Parser coverage for the extended instruction set. The same mnemonic can back
# several forms (`cmp`/`cmn` register vs immediate; `cset`/`csel` per
# condition, spelled as a literal in the asm template); the parser
# disambiguates by trying each candidate. x1 is pointed at the default test
# memory window so the loads execute cleanly.
.global done
done:
  add x0, x0, x0
.global fall
fall:
  add x11, x0, 9
.global _start
_start:
  movz x1, 32768
  add x2, x0, 16
  lslv x1, x1, x2
  movz x3, 4096
  add x1, x1, x3
  mul x4, x2, x3
  madd x5, x2, x3, x4
  cmp x2, 5
  cmn x2, x3
  tst x2, x3
  cset x6, eq
  csel x7, x2, x3, ne
  ldur x8, [x1, -8]
  ldp x9, x10, [x1, 16]
  stur x8, [x1, 32]
  tbz x2, 4, 2

# CHECK: movz rd=GPR[1], imm=32768
# CHECK-NEXT: add rd=GPRsp[2], rn=GPRsp[0], imm=16
# CHECK-NEXT: lslv rd=GPR[1], rn=GPR[1], rm=GPR[2]
# CHECK-NEXT: movz rd=GPR[3], imm=4096
# CHECK-NEXT: add rd=GPR[1], rn=GPR[1], rm=GPR[3]
# CHECK-NEXT: mul rd=GPR[4], rn=GPR[2], rm=GPR[3]
# CHECK-NEXT: madd rd=GPR[5], rn=GPR[2], rm=GPR[3], ra=GPR[4]
# CHECK-NEXT: cmp rn=GPRsp[2], imm=5
# CHECK-NEXT: cmn rn=GPR[2], rm=GPR[3]
# CHECK-NEXT: tst rn=GPR[2], rm=GPR[3]
# CHECK-NEXT: cset rd=GPR[6]
# CHECK-NEXT: csel rd=GPR[7], rn=GPR[2], rm=GPR[3]
# CHECK-NEXT: ldur rt=GPR[8], rn=GPRsp[1], imm=-8
# CHECK-NEXT: ldp rt=GPR[9], rt2=GPR[10], rn=GPRsp[1], imm=16
# CHECK-NEXT: stur rt=GPR[8], rn=GPRsp[1], imm=32
# CHECK-NEXT: tbz rt=GPR[2], bit=4, imm=2
