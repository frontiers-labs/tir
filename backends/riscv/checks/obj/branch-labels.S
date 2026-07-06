# RUN: tir mc --march=rv64i --filetype=obj-ascii %s | filecheck %s

.global count
count:
    addi x1, x0, 0
back:
    addi x1, x1, 1
    bne  x1, x2, back
    beq  x1, x2, done
    addi x3, x0, 5
done:
    jalr x0, 0(x1)

# Layout (4-byte fixed instructions), branch displacements are pc-relative from
# the branch instruction's own address:
#   0x00  addi x1, x0, 0     count
#   0x04  addi x1, x1, 1     back
#   0x08  bne  x1, x2, back  -> 0x04 - 0x08 = -4, B-type imm = 0xFE209EE3
#   0x0C  beq  x1, x2, done  -> 0x14 - 0x0C = +8, B-type imm = 0x00208463
#   0x10  addi x3, x0, 5
#   0x14  jalr x0, 0(x1)     done
# CHECK: .section .text
# CHECK-NEXT: count:
# CHECK-NEXT: [0x93, 0x00, 0x00, 0x00]
# CHECK-NEXT: [0x93, 0x80, 0x10, 0x00]
# CHECK-NEXT: [0xE3, 0x9E, 0x20, 0xFE]
# CHECK-NEXT: [0x63, 0x84, 0x20, 0x00]
# CHECK-NEXT: [0x93, 0x01, 0x50, 0x00]
# CHECK-NEXT: [0x67, 0x80, 0x00, 0x00]
