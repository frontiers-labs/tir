# RUN: tir mc --march=x86_64 --filetype=obj-ascii %s | filecheck %s

.global ordering
ordering:
    sfence
    lfence
    mfence
    pause

# CHECK: ordering:
# CHECK-NEXT: [0x0F, 0xAE, 0xF8]
# CHECK-NEXT: [0x0F, 0xAE, 0xE8]
# CHECK-NEXT: [0x0F, 0xAE, 0xF0]
# CHECK-NEXT: [0xF3, 0x90]
