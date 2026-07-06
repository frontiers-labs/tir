// RUN: tir mc --march=arm64 --filetype=obj -o - %s | tir readobj - | filecheck %s

.global caller
caller:
    b.gt target
    cbz x0, target
    tbz x0, 3, target
    ret x30

// Each branch targets an undefined symbol: the pc-relative immediate stays zero
// and a relocation is emitted against it. b.<cond> and cbz use CONDBR19; tbz
// uses TSTBR14.
// CHECK: Symbol target: value=0x0 size=0x0 bind=GLOBAL type=NOTYPE section=UND
// CHECK: Reloc .text+0x0: R_AARCH64_CONDBR19 target + 0
// CHECK: Reloc .text+0x4: R_AARCH64_CONDBR19 target + 0
// CHECK: Reloc .text+0x8: R_AARCH64_TSTBR14 target + 0
