# RUN: isasim snippet --march=riscv64 --entry=_start --until-pc=done --engine=interp --events=- %s | filecheck %s

.global done
done:
  add x0, x0, x0
.global _start
_start:
  lw x2, 0(x10)

# CHECK: "engine": "interp"
# CHECK: "kind": "read"
# CHECK: "pc": 2147483648
# CHECK: "sequence": 0
# CHECK: "status": "complete"
