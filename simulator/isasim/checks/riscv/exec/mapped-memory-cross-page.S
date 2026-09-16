# RUN: not isasim --march=riscv64 --entry=_start --until-pc=done --engine=interp --mem-size=0 --memory-config=%S/Inputs/mapped-memory-cross-page.json --events=- %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lui x10, 0x101
  addi x10, x10, -1
  sw x0, 0(x10)

# CHECK: "kind": "write"
# CHECK: unmapped
# CHECK: "status": "fault"
