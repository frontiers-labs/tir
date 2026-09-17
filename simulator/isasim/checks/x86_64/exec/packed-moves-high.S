# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  ret
.global _start
_start:
  movabs rax, 0x80001000
  movups xmm8, [rax + 0]
  movhps xmm8, [rax + 24]
  movhps [rax + 64], xmm8
  movups xmm9, [rax + 0]
  movlps xmm9, [rax + 64]

# CHECK: final registers:
# CHECK-DAG: XMM128[8] = 0x1f1e1d1c1b1a19180706050403020100
# CHECK-DAG: XMM128[9] = 0xf0e0d0c0b0a09081f1e1d1c1b1a1918
