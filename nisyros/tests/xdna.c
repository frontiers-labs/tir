#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

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

static void fill(nisyros_device device, nisyros_queue queue, nisyros_addr address,
                 uint32_t pattern) {
    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(device, 0, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(command, address, 4096, pattern) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit(queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count >= 2 && count <= 32);
    nisyros_device_id ids[32];
    uint32_t capacity = 32;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_SUCCESS);
    nisyros_device_id gpu_id = {0}, xdna_id = {0};
    for (uint32_t i = 0; i < capacity; ++i) {
        if (ids[i].backend == NISYROS_BACKEND_ROCM) gpu_id = ids[i];
        if (ids[i].backend == NISYROS_BACKEND_XDNA) xdna_id = ids[i];
    }
    assert(gpu_id.backend && xdna_id.backend);
    nisyros_queue gpu_queue = NULL, xdna_queue = NULL;
    nisyros_device gpu = open_device(cluster, gpu_id, &gpu_queue);
    nisyros_device xdna = open_device(cluster, xdna_id, &xdna_queue);

    nisyros_alloc_desc alloc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    alloc.bytes = 4096;
    alloc.alignment = 4096;
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    nisyros_addr xdna_base = 0, local_base = 0, gpu_base = 0;
    assert(nisyros_alloc(xdna, &alloc, &xdna_base) == NISYROS_SUCCESS);
    fill(xdna, xdna_queue, xdna_base, 0xabcdef01);
    assert(nisyros_invalidate(xdna, xdna_base, 4096) == NISYROS_SUCCESS);
    uint32_t *host = NULL;
    assert(nisyros_map(xdna, xdna_base, 4096, (void **)&host) == NISYROS_SUCCESS);
    for (int i = 0; i < 1024; ++i) assert(host[i] == 0xabcdef01);
    assert(nisyros_unmap(xdna, host) == NISYROS_SUCCESS);

    alloc.required_properties = NISYROS_MEMORY_DEVICE_LOCAL;
    assert(nisyros_alloc(xdna, &alloc, &local_base) == NISYROS_SUCCESS);
    fill(xdna, xdna_queue, local_base, 0x89abcdef);
    assert(nisyros_invalidate(xdna, local_base, 4096) == NISYROS_SUCCESS);
    assert(nisyros_map(xdna, local_base, 4096, (void **)&host) == NISYROS_SUCCESS);
    for (int i = 0; i < 1024; ++i) assert(host[i] == 0x89abcdef);
    assert(nisyros_unmap(xdna, host) == NISYROS_SUCCESS);
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;

    FILE *input = fopen(argv[1], "rb");
    assert(input);
    assert(fseek(input, 0, SEEK_END) == 0);
    long length = ftell(input);
    assert(length > 0 && length <= 128 * 1024 * 1024);
    rewind(input);
    void *bytes = malloc((size_t)length);
    assert(bytes && fread(bytes, 1, (size_t)length, input) == (size_t)length);
    assert(fclose(input) == 0);
    nisyros_image image = NISYROS_DESC_INIT(nisyros_image);
    image.format = NISYROS_IMAGE_XDNA_XCLBIN;
    image.byte_size = (uint64_t)length;
    image.bytes = bytes;
    nisyros_executable executable = NULL;
    assert(nisyros_executable_load(xdna, &image, &executable) == NISYROS_SUCCESS);
    assert(nisyros_executable_destroy(executable) == NISYROS_SUCCESS);
    free(bytes);

    assert(nisyros_alloc(gpu, &alloc, &gpu_base) == NISYROS_SUCCESS);
    nisyros_peer_info route = NISYROS_DESC_INIT(nisyros_peer_info);
    assert(nisyros_peer_capabilities(xdna, gpu, &route) == NISYROS_SUCCESS);
    assert(!route.same_physical_device && route.direct_mapping && !route.event_import);
    assert(route.shared_allocation_backend == NISYROS_BACKEND_ROCM);
    assert(nisyros_peer_capabilities(gpu, xdna, &route) == NISYROS_SUCCESS);
    assert(!route.direct_mapping);
    assert(route.shared_allocation_backend == NISYROS_BACKEND_ROCM);
    struct {
        nisyros_descriptor_header header;
        uint32_t same_physical_device;
        uint32_t direct_mapping;
        uint32_t event_import;
        uint32_t host_staging;
        uint32_t canary;
    } legacy = {{0}, 0, 0, 0, 0, 0x12345678};
    legacy.header.struct_size = sizeof(legacy) - sizeof(legacy.canary);
    legacy.header.abi_version = NISYROS_ABI_VERSION;
    assert(nisyros_peer_capabilities(xdna, gpu,
                                     (nisyros_peer_info *)&legacy) == NISYROS_SUCCESS);
    assert(legacy.direct_mapping && legacy.canary == 0x12345678);
    nisyros_addr imported = 0;
    nisyros_location source = {gpu, gpu_base};
    assert(nisyros_peer_map(xdna, source, 4096, NISYROS_ACCESS_WRITE,
                            &imported) == NISYROS_SUCCESS);
    fill(xdna, xdna_queue, imported, 0x12345678);
    assert(nisyros_invalidate(gpu, gpu_base, 4096) == NISYROS_SUCCESS);
    assert(nisyros_map(gpu, gpu_base, 4096, (void **)&host) == NISYROS_SUCCESS);
    for (int i = 0; i < 1024; ++i) assert(host[i] == 0x12345678);
    assert(nisyros_unmap(gpu, host) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(xdna, imported) == NISYROS_SUCCESS);
    assert(nisyros_free(gpu, gpu_base) == NISYROS_SUCCESS);
    assert(nisyros_free(xdna, local_base) == NISYROS_SUCCESS);
    assert(nisyros_free(xdna, xdna_base) == NISYROS_SUCCESS);
    assert(nisyros_device_close(xdna) == NISYROS_SUCCESS);
    assert(nisyros_device_close(gpu) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("XDNA discovery, memory, XCLBIN CU configuration and GPU sharing verified");
    return 0;
}
