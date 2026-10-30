#include <hip/hip_runtime.h>
#include <stdint.h>

struct Arguments {
    uint32_t *destination;
    const uint32_t *source;
    uint32_t count;
    uint32_t padding;
};

extern "C" __global__ void nisyros_add_one(const Arguments *arguments) {
    uint32_t index = blockIdx.x * blockDim.x + threadIdx.x;
    if (index < arguments->count)
        arguments->destination[index] = arguments->source[index] + 1;
}
