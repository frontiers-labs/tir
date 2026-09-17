# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, 0x80001000
  movups xmm0, [rax + 0]
  movaps xmm8, xmm0
  movapd xmm9, xmm8
  movupd xmm10, xmm9
  movdqu xmm11, xmm10
  movups xmm12, xmm11

# CHECK: final registers:
# CHECK-DAG: XMM128[8] = 0xf0e0d0c0b0a09080706050403020100
# CHECK-DAG: XMM128[9] = 0xf0e0d0c0b0a09080706050403020100
# CHECK-DAG: XMM128[10] = 0xf0e0d0c0b0a09080706050403020100
# CHECK-DAG: XMM128[11] = 0xf0e0d0c0b0a09080706050403020100
# CHECK-DAG: XMM128[12] = 0xf0e0d0c0b0a09080706050403020100
