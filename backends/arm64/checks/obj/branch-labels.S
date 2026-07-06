// RUN: tir mc --march=arm64 --filetype=obj-ascii %s | filecheck %s

.global count
count:
    movz x1, 0
back:
    add x1, x1, 1
    b.ne back
    b done
    movz x3, 5
done:
    ret x30

// Layout (4-byte fixed instructions), branch displacements are pc-relative from
// the branch's own address and scaled by 4 (word offsets):
//   0x00  movz x1, 0     count
//   0x04  add  x1, x1, 1 back
//   0x08  b.ne back  -> (0x04 - 0x08) >> 2 = -1, imm19 = 0x7FFFF -> 0x54FFFFE1
//   0x0C  b    done  -> (0x14 - 0x0C) >> 2 = +2, imm26 = 0x2     -> 0x14000002
//   0x10  movz x3, 5
//   0x14  ret  x30       done
// CHECK: .section .text
// CHECK-NEXT: count:
// CHECK-NEXT: [0x01, 0x00, 0x80, 0xD2]
// CHECK-NEXT: [0x21, 0x04, 0x00, 0x91]
// CHECK-NEXT: [0xE1, 0xFF, 0xFF, 0x54]
// CHECK-NEXT: [0x02, 0x00, 0x00, 0x14]
// CHECK-NEXT: [0xA3, 0x00, 0x80, 0xD2]
// CHECK-NEXT: [0xC0, 0x03, 0x5F, 0xD6]
