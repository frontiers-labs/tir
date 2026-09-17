# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, 0x80001000
  movups xmm8, [rax + 0]
  movups xmm9, [rax + 0]
  movlps xmm8, [rax + 24]
  movlpd xmm9, [rax + 40]
  movlps [rax + 64], xmm8
  movlpd [rax + 72], xmm9
  movups xmm10, [rax + 64]

# CHECK: final registers:
# CHECK-DAG: XMM128[8] = 0xf0e0d0c0b0a09081f1e1d1c1b1a1918
# CHECK-DAG: XMM128[9] = 0xf0e0d0c0b0a09082f2e2d2c2b2a2928
# CHECK-DAG: XMM128[10] = 0x2f2e2d2c2b2a29281f1e1d1c1b1a1918
