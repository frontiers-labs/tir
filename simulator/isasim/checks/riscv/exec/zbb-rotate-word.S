# RUN: isasim --march=rv64i_zbb --entry=_start --until-pc=done --trace-registers-end %s | filecheck %s

.global done
done:
    add x0, x0, x0
.global _start
_start:
    addi x1, x0, 1
    addi x2, x0, 32
    rorw x3, x1, x1
    rolw x4, x3, x1
    roriw x5, x1, 31
    rorw x6, x3, x2
    rolw x7, x3, x2
    roriw x8, x3, 0
    slli x9, x1, 32
    rorw x12, x9, x1
    rolw x13, x9, x1
    roriw x14, x9, 31
    rori x15, x1, 63
    rori x16, x1, 32
    addi x17, x0, -1
    rorw x18, x1, x17
    rolw x19, x1, x17

# CHECK: final registers:
# CHECK-DAG: GPR[3] = 0xffffffff80000000 (
# CHECK-DAG: GPR[4] = 0x1 (
# CHECK-DAG: GPR[5] = 0x2 (
# CHECK-DAG: GPR[6] = 0xffffffff80000000 (
# CHECK-DAG: GPR[7] = 0xffffffff80000000 (
# CHECK-DAG: GPR[8] = 0xffffffff80000000 (
# CHECK-DAG: GPR[12] = 0x0 (
# CHECK-DAG: GPR[13] = 0x0 (
# CHECK-DAG: GPR[14] = 0x0 (
# CHECK-DAG: GPR[15] = 0x2 (
# CHECK-DAG: GPR[16] = 0x100000000 (
# CHECK-DAG: GPR[18] = 0x2 (
# CHECK-DAG: GPR[19] = 0xffffffff80000000 (
