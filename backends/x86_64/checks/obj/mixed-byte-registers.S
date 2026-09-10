# This file was generated with ./utils/scripts/update_checks.py. Do not modify CHECKs manually.

# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

.global mixed
mixed:
  mov bh, al
  mov al, bh
  mov ah, al
  mov al, ah
  add ch, dl
  add dl, ch
  sub dh, bl
  sub bl, dh
  and ah, cl
  and cl, ah
  or bh, dl
  or dl, bh
  xor ch, bl
  xor bl, ch

# CHECK: .section .text
# CHECK-NEXT: mixed:
# CHECK-NEXT:   [0x88, 0xC7]
# CHECK-NEXT:   [0x88, 0xF8]
# CHECK-NEXT:   [0x88, 0xC4]
# CHECK-NEXT:   [0x88, 0xE0]
# CHECK-NEXT:   [0x00, 0xD5]
# CHECK-NEXT:   [0x00, 0xEA]
# CHECK-NEXT:   [0x28, 0xDE]
# CHECK-NEXT:   [0x28, 0xF3]
# CHECK-NEXT:   [0x20, 0xCC]
# CHECK-NEXT:   [0x20, 0xE1]
# CHECK-NEXT:   [0x08, 0xD7]
# CHECK-NEXT:   [0x08, 0xFA]
# CHECK-NEXT:   [0x30, 0xDD]
# CHECK-NEXT:   [0x30, 0xEB]
