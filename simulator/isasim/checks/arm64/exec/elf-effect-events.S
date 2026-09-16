# RUN: not isasim --march=arm64 --events=- %S/Inputs/load-fault.elf | filecheck %s

# The ELF64 fixture contains ldr x0, [x0] at 0x1000. x0 starts at zero,
# outside the compatibility memory mapping.
# CHECK: "engine": "interp"
# CHECK: "kind": "read"
# CHECK: "pc": 4096
# CHECK: "sequence": 0
# CHECK: "fault": {
# CHECK: "access": "read"
# CHECK: "reason": "unmapped"
# CHECK: "pc": 4096
# CHECK: "status": "fault"
