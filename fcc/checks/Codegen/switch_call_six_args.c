// RUN: fcc compile --stage asm --march x86_64 -o - %s | filecheck %s

struct matrix
{
    int size;
};

struct results
{
    short seed1;
    short seed2;
    short seed3;
    void *memblock[4];
    unsigned size;
    struct matrix mat;
    unsigned short crc;
    unsigned short crcmatrix;
    unsigned short crcstate;
};

extern unsigned short state_benchmark(
    unsigned, unsigned char *, short, short, short, unsigned short);
extern unsigned short matrix_benchmark(
    struct matrix *, short, unsigned short);
extern unsigned short crc16(unsigned short, unsigned short);

short switch_call_six_args(short *data_ptr, struct results *res)
{
    short data = *data_ptr;
    short result;
    unsigned char cached = (data >> 7) & 1;
    if (cached)
        return data & 127;

    short flag = data & 7;
    short value = (data >> 3) & 15;
    value |= value << 4;
    switch (flag)
    {
        case 0:
            result = state_benchmark(res->size,
                                     res->memblock[3],
                                     res->seed1,
                                     res->seed2,
                                     value,
                                     res->crc);
            if (res->crcstate == 0)
                res->crcstate = result;
            break;
        case 1:
            result = matrix_benchmark(&res->mat, value, res->crc);
            if (res->crcmatrix == 0)
                res->crcmatrix = result;
            break;
        default:
            result = data;
            break;
    }
    res->crc = crc16(result, res->crc);
    result &= 127;
    *data_ptr = (data & 0xff00) | 0x80 | result;
    return result;
}

// CHECK: switch_call_six_args:
// CHECK: call state_benchmark
