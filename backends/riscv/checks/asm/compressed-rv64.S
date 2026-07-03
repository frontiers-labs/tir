# RUN: tir mc --march=rv64gc %s | filecheck %s
# RUN: tir mc --march=rv64imd_zicsr --mattr=+c %s | filecheck %s
# RUN: tir mc --march=rv64gc --filetype=obj-ascii %s | filecheck %s --check-prefix=ENC

# The RV64C instruction set (plus the Zcd double loads/stores), with encodings
# cross-checked against the GNU assembler.

.global f
f:
    c.addi4spn a0, sp, 16
    c.lw a1, 4(s0)
    c.ld a1, 8(s0)
    c.sw a1, 12(s0)
    c.sd a1, 16(s0)
    c.fld fa0, 8(s0)
    c.fsd fa0, 16(s0)
    c.nop
    c.addi a0, -1
    c.addiw a0, 5
    c.li a2, 31
    c.addi16sp sp, -32
    c.lui a3, 1
    c.srli s0, 2
    c.srai s1, 3
    c.andi s0, 15
    c.sub a0, a1
    c.xor a0, a1
    c.or a0, a1
    c.and a0, a1
    c.subw a0, a1
    c.addw a0, a1
    c.j 0
    c.beqz a0, -12
    c.bnez a0, 8
    c.slli a0, 4
    c.lwsp a0, 8(sp)
    c.ldsp a1, 16(sp)
    c.fldsp fa1, 24(sp)
    c.jr ra
    c.mv a0, a1
    c.ebreak
    c.jalr a0
    c.add a0, a1
    c.swsp a0, 20(sp)
    c.sdsp a1, 24(sp)
    c.fsdsp fa1, 32(sp)

# CHECK: c.addi4spn x10, sp, 16
# CHECK-NEXT: c.lw x11, 4(x8)
# CHECK-NEXT: c.ld x11, 8(x8)
# CHECK-NEXT: c.sw x11, 12(x8)
# CHECK-NEXT: c.sd x11, 16(x8)
# CHECK-NEXT: c.fld f10, 8(x8)
# CHECK-NEXT: c.fsd f10, 16(x8)
# CHECK-NEXT: c.nop
# CHECK-NEXT: c.addi x10, -1
# CHECK-NEXT: c.addiw x10, 5
# CHECK-NEXT: c.li x12, 31
# CHECK-NEXT: c.addi16sp sp, -32
# CHECK-NEXT: c.lui x13, 1
# CHECK-NEXT: c.srli x8, 2
# CHECK-NEXT: c.srai x9, 3
# CHECK-NEXT: c.andi x8, 15
# CHECK-NEXT: c.sub x10, x11
# CHECK-NEXT: c.xor x10, x11
# CHECK-NEXT: c.or x10, x11
# CHECK-NEXT: c.and x10, x11
# CHECK-NEXT: c.subw x10, x11
# CHECK-NEXT: c.addw x10, x11
# CHECK-NEXT: c.j 0
# CHECK-NEXT: c.beqz x10, -12
# CHECK-NEXT: c.bnez x10, 8
# CHECK-NEXT: c.slli x10, 4
# CHECK-NEXT: c.lwsp x10, 8(sp)
# CHECK-NEXT: c.ldsp x11, 16(sp)
# CHECK-NEXT: c.fldsp f11, 24(sp)
# CHECK-NEXT: c.jr x1
# CHECK-NEXT: c.mv x10, x11
# CHECK-NEXT: c.ebreak
# CHECK-NEXT: c.jalr x10
# CHECK-NEXT: c.add x10, x11
# CHECK-NEXT: c.swsp x10, 20(sp)
# CHECK-NEXT: c.sdsp x11, 24(sp)
# CHECK-NEXT: c.fsdsp f11, 32(sp)

# ENC: [0x08, 0x08]
# ENC-NEXT: [0x4C, 0x40]
# ENC-NEXT: [0x0C, 0x64]
# ENC-NEXT: [0x4C, 0xC4]
# ENC-NEXT: [0x0C, 0xE8]
# ENC-NEXT: [0x08, 0x24]
# ENC-NEXT: [0x08, 0xA8]
# ENC-NEXT: [0x01, 0x00]
# ENC-NEXT: [0x7D, 0x15]
# ENC-NEXT: [0x15, 0x25]
# ENC-NEXT: [0x7D, 0x46]
# ENC-NEXT: [0x3D, 0x71]
# ENC-NEXT: [0x85, 0x66]
# ENC-NEXT: [0x09, 0x80]
# ENC-NEXT: [0x8D, 0x84]
# ENC-NEXT: [0x3D, 0x88]
# ENC-NEXT: [0x0D, 0x8D]
# ENC-NEXT: [0x2D, 0x8D]
# ENC-NEXT: [0x4D, 0x8D]
# ENC-NEXT: [0x6D, 0x8D]
# ENC-NEXT: [0x0D, 0x9D]
# ENC-NEXT: [0x2D, 0x9D]
# ENC-NEXT: [0x01, 0xA0]
# ENC-NEXT: [0x75, 0xD9]
# ENC-NEXT: [0x01, 0xE5]
# ENC-NEXT: [0x12, 0x05]
# ENC-NEXT: [0x22, 0x45]
# ENC-NEXT: [0xC2, 0x65]
# ENC-NEXT: [0xE2, 0x25]
# ENC-NEXT: [0x82, 0x80]
# ENC-NEXT: [0x2E, 0x85]
# ENC-NEXT: [0x02, 0x90]
# ENC-NEXT: [0x02, 0x95]
# ENC-NEXT: [0x2E, 0x95]
# ENC-NEXT: [0x2A, 0xCA]
# ENC-NEXT: [0x2E, 0xEC]
# ENC-NEXT: [0x2E, 0xB0]
