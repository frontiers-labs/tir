// RUN: fcc compile --stage ir -o - %s | filecheck %s --check-prefix=IR
// RUN: fcc compile --stage asm --march arm64 -o - %s | filecheck %s --check-prefix=ASM

typedef unsigned int ee_u32;
typedef unsigned char ee_u8;
typedef signed short ee_s16;

void corrupt(ee_u32 blksize, ee_u8 *memblock, ee_s16 seed, ee_s16 step)
{
    ee_u8 *p = memblock;
    while (p < (memblock + blksize))
    {
        if (*p != 44)
            *p ^= (ee_u8)seed;
        p += step;
    }
}

// IR: ptr.ptradd
// IR-NOT: addi {{.*}} : !ptr

// ASM: corrupt:
// ASM: ldrb
// ASM: strb
