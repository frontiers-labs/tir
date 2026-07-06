// RUN: tir mc --march=x86_64 --filetype=obj -o - %s | tir readobj - | filecheck %s

.global caller
caller:
    jmp target
    je target
    ret

// The branch targets an undefined symbol: the disp32 stays zero and a
// pc-relative relocation is emitted at the displacement field of each branch
// (jmp: opcode + 1; jcc: two-byte opcode + 2), with addend -4 because the
// displacement is measured from the end of the instruction.
// CHECK: Symbol target: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
// CHECK: Reloc .text+0x1: R_X86_64_PC32 target + -4
// CHECK: Reloc .text+0x7: R_X86_64_PC32 target + -4
