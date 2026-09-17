# RUN: isasim --march=x86_64 --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
  add rax, rax
.global _start
_start:
  movabs rax, 0x80001000
  movupd xmm8, [rax + 1]
  movdqu xmm9, [rax + 17]
  movupd [rax + 65], xmm9
  movdqu [rax + 81], xmm8
  movups xmm10, [rax + 65]
  movups xmm11, [rax + 81]
  movups [rax + 97], xmm9
  movupd xmm12, [rax + 97]

# CHECK: final registers:
# CHECK-DAG: XMM128[8] = 0x100f0e0d0c0b0a090807060504030201
# CHECK-DAG: XMM128[9] = 0x201f1e1d1c1b1a191817161514131211
# CHECK-DAG: XMM128[10] = 0x201f1e1d1c1b1a191817161514131211
# CHECK-DAG: XMM128[11] = 0x100f0e0d0c0b0a090807060504030201
# CHECK-DAG: XMM128[12] = 0x201f1e1d1c1b1a191817161514131211
