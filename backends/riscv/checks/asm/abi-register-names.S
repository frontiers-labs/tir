# RUN: tir mc --march=rv64i --stage=isel %s | filecheck %s

# ABI register names resolve to their numbered GPRs (a0/a1/a2 -> x10/x11/x12).

.global foo
entry:
    add a0, a1, a2

# CHECK:      module {
# CHECK-NEXT:   asm.section {
# CHECK-NEXT:     asm.symbol {name = "foo"} {
# CHECK-NEXT:       riscv.add {rd = x10, rs1 = x11, rs2 = x12}
# CHECK-NEXT:       asm.symbol_end
# CHECK-NEXT:     }
# CHECK-NEXT:   }
# CHECK-NEXT:   module_end
# CHECK-NEXT: }
