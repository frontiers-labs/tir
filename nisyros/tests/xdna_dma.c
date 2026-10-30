#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void *read_file(const char *path, size_t *length) {
    FILE *file = fopen(path, "rb");
    assert(file);
    assert(fseek(file, 0, SEEK_END) == 0);
    long size = ftell(file);
    assert(size > 0);
    rewind(file);
    void *data = malloc((size_t)size);
    assert(data && fread(data, 1, (size_t)size, file) == (size_t)size);
    assert(fclose(file) == 0);
    *length = (size_t)size;
    return data;
}

static nisyros_device open_device(nisyros_cluster cluster, nisyros_device_id id,
                                  nisyros_queue *queue) {
    nisyros_queue_request request = {0, 1, 1.0f, 0};
    nisyros_device_open_desc desc = NISYROS_DESC_INIT(nisyros_device_open_desc);
    desc.queue_request_count = 1;
    desc.queue_requests = &request;
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &desc, &device) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(device, 0, 0, queue) == NISYROS_SUCCESS);
    return device;
}

static nisyros_addr allocate(nisyros_device device, uint64_t bytes,
                             uint32_t properties) {
    nisyros_alloc_desc desc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    desc.bytes = bytes;
    desc.alignment = 4096;
    desc.required_properties = properties;
    nisyros_addr address = 0;
    assert(nisyros_alloc(device, &desc, &address) == NISYROS_SUCCESS);
    return address;
}

static void patch_shim_dma(uint8_t *code, size_t offset, nisyros_addr address) {
    uint32_t *words = (uint32_t *)(code + offset);
    uint64_t base = ((uint64_t)(words[2] & 0xffff) << 32) | words[1];
    base += address + UINT64_C(0x80000000);
    words[1] = (uint32_t)(base & UINT32_C(0xfffffffc));
    words[2] = (words[2] & UINT32_C(0xffff0000)) | (uint32_t)(base >> 32);
}

int main(int argc, char **argv) {
    assert(argc == 3);
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    nisyros_device_id ids[8];
    uint32_t count = 8;
    assert(nisyros_cluster_devices(cluster, &count, ids) == NISYROS_SUCCESS);
    nisyros_device_id gpu_id = {0}, xdna_id = {0};
    for (uint32_t i = 0; i < count; ++i) {
        if (ids[i].backend == NISYROS_BACKEND_ROCM) gpu_id = ids[i];
        if (ids[i].backend == NISYROS_BACKEND_XDNA) xdna_id = ids[i];
    }
    assert(gpu_id.backend && xdna_id.backend);
    nisyros_queue gpu_queue = NULL, xdna_queue = NULL;
    nisyros_device gpu = open_device(cluster, gpu_id, &gpu_queue);
    nisyros_device xdna = open_device(cluster, xdna_id, &xdna_queue);
    nisyros_peer_info route = NISYROS_DESC_INIT(nisyros_peer_info);
    assert(nisyros_peer_capabilities(xdna, gpu, &route) == NISYROS_SUCCESS);
    assert(route.direct_mapping && route.shared_allocation_backend == NISYROS_BACKEND_ROCM);
    assert(nisyros_peer_capabilities(gpu, xdna, &route) == NISYROS_SUCCESS);
    assert(!route.direct_mapping && route.shared_allocation_backend == NISYROS_BACKEND_ROCM);

    size_t package_bytes = 0, code_bytes = 0;
    void *package = read_file(argv[1], &package_bytes);
    uint8_t *code = read_file(argv[2], &code_bytes);
    assert(code_bytes == 1120400);
    const uint32_t *source_patch = (const uint32_t *)(code + 0xe0);
    const uint32_t *destination_patch = (const uint32_t *)(code + 0x140);
    assert(source_patch[0] == 1 && source_patch[1] == 0 && source_patch[2] == 0);
    assert(destination_patch[0] == 1 && destination_patch[1] == 0 && destination_patch[2] == 0);
    nisyros_image image = NISYROS_DESC_INIT(nisyros_image);
    image.format = NISYROS_IMAGE_XDNA_XCLBIN;
    image.byte_size = package_bytes;
    image.bytes = package;
    nisyros_executable executable = NULL;
    assert(nisyros_executable_load(xdna, &image, &executable) == NISYROS_SUCCESS);
    nisyros_kernel kernel = NULL;
    assert(nisyros_kernel_lookup(executable, "DPU", &kernel) == NISYROS_SUCCESS);

    const size_t bytes = 1024 * 1024;
    nisyros_addr input = allocate(gpu, bytes, NISYROS_MEMORY_HOST_VISIBLE);
    nisyros_addr output = allocate(gpu, bytes, NISYROS_MEMORY_HOST_VISIBLE);
    uint8_t *host = NULL;
    assert(nisyros_map(gpu, input, bytes, (void **)&host) == NISYROS_SUCCESS);
    memset(host, 0x5a, bytes);
    assert(nisyros_flush(gpu, input, bytes) == NISYROS_SUCCESS);
    assert(nisyros_unmap(gpu, host) == NISYROS_SUCCESS);
    assert(nisyros_map(gpu, output, bytes, (void **)&host) == NISYROS_SUCCESS);
    memset(host, 0, bytes);
    assert(nisyros_flush(gpu, output, bytes) == NISYROS_SUCCESS);
    assert(nisyros_unmap(gpu, host) == NISYROS_SUCCESS);

    nisyros_addr xdna_input = 0, xdna_output = 0;
    nisyros_location source = {gpu, input};
    int mapping = nisyros_peer_map(xdna, source, bytes, NISYROS_ACCESS_READ,
                                   &xdna_input);
    assert(mapping == NISYROS_SUCCESS);
    source.address = output;
    mapping = nisyros_peer_map(xdna, source, bytes, NISYROS_ACCESS_WRITE,
                               &xdna_output);
    assert(mapping == NISYROS_SUCCESS);
    nisyros_addr instructions = allocate(xdna, code_bytes,
                                         NISYROS_MEMORY_DEVICE_LOCAL);
    uint8_t *mapped_code = NULL;
    assert(nisyros_map(xdna, instructions, code_bytes,
                       (void **)&mapped_code) == NISYROS_SUCCESS);
    memcpy(mapped_code, code, code_bytes);
    patch_shim_dma(mapped_code, 0xe0, xdna_input);
    patch_shim_dma(mapped_code, 0x140, xdna_output);
    assert(nisyros_flush(xdna, instructions, code_bytes) == NISYROS_SUCCESS);
    assert(nisyros_unmap(xdna, mapped_code) == NISYROS_SUCCESS);

    struct {
        uint64_t instruction;
        uint32_t instruction_bytes;
        uint32_t property_words;
        uint64_t input;
        uint64_t output;
    } arguments = {instructions, (uint32_t)code_bytes, 0, xdna_input, xdna_output};
    nisyros_grid grid = {1, 1, 1};
    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(xdna, 0, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch(command, kernel, &grid, &arguments,
                                sizeof(arguments)) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit(xdna_queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);

    assert(nisyros_invalidate(gpu, output, bytes) == NISYROS_SUCCESS);
    assert(nisyros_map(gpu, output, bytes, (void **)&host) == NISYROS_SUCCESS);
    for (size_t i = 0; i < 4; ++i) assert(host[i] == 0x5a);
    for (size_t i = 4; i < bytes; ++i) assert(host[i] == 0);
    assert(nisyros_unmap(gpu, host) == NISYROS_SUCCESS);

    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(xdna, xdna_input) == NISYROS_SUCCESS);
    assert(nisyros_cmd_create(xdna, 0, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(command, xdna_output, 16, 0x12345678) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    assert(nisyros_submit(xdna_queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(gpu, output, bytes) == NISYROS_SUCCESS);
    assert(nisyros_map(gpu, output, bytes, (void **)&host) == NISYROS_SUCCESS);
    for (size_t i = 0; i < 4; ++i) assert(((uint32_t *)host)[i] == 0x12345678);
    assert(nisyros_unmap(gpu, host) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
    assert(nisyros_free(xdna, instructions) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(xdna, xdna_output) == NISYROS_SUCCESS);
    assert(nisyros_free(gpu, output) == NISYROS_SUCCESS);
    assert(nisyros_free(gpu, input) == NISYROS_SUCCESS);
    assert(nisyros_executable_destroy(executable) == NISYROS_SUCCESS);
    assert(nisyros_device_close(xdna) == NISYROS_SUCCESS);
    assert(nisyros_device_close(gpu) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    free(code);
    free(package);
    puts("XDNA DMA copied 4 bytes between GPU-owned buffers");
    return 0;
}
