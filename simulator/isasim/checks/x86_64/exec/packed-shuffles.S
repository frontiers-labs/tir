# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  ret
.global _start
_start:
  movabs rax, 0x80001000
  movups xmm0, [rax + 0]
  movups xmm1, [rax + 16]
  shufps xmm0, xmm1, 0x1b
  movups xmm2, [rax + 0]
  movups xmm3, [rax + 16]
  shufpd xmm2, xmm3, 1

# CHECK: final registers:
# CHECK-DAG: XMM128[0] = 0x13121110171615140b0a09080f0e0d0c
# CHECK-DAG: XMM128[2] = 0x17161514131211100f0e0d0c0b0a0908
