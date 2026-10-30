#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    nisyros_addr destination;
    nisyros_addr source;
    uint32_t count;
    uint32_t padding;
} arguments;

_Static_assert(sizeof(arguments) == 24, "kernel argument layout");

static uint64_t checksum64(const uint8_t *bytes, size_t size) {
    uint64_t hash = UINT64_C(0xcbf29ce484222325);
    for (size_t i = 0; i < size; ++i) {
        hash = (hash ^ bytes[i]) * UINT64_C(0x100000001b3);
    }
    return hash;
}

int main(int argc, char **argv) {
    assert(argc == 2);
    FILE *file = fopen(argv[1], "rb");
    assert(file != NULL);
    assert(fseek(file, 0, SEEK_END) == 0);
    long size = ftell(file);
    assert(size > 0);
    assert(fseek(file, 0, SEEK_SET) == 0);
    uint8_t *package = malloc((size_t)size);
    assert(package != NULL);
    assert(fread(package, 1, (size_t)size, file) == (size_t)size);
    assert(fclose(file) == 0);

    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    nisyros_device_id id;
    uint32_t count = 1;
    assert(nisyros_cluster_devices(cluster, &count, &id) == NISYROS_SUCCESS);
    assert(count == 1);
    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    assert(info.strict_profile_supported == 1);
    assert(info.device_type != NISYROS_DEVICE_TYPE_CPU);
    uint32_t family = UINT32_MAX;
    for (uint32_t index = 0; index < info.queue_family_count; ++index) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, index, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_COMPUTE) { family = index; break; }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &open, &device) == NISYROS_SUCCESS);
    nisyros_queue queue = NULL;
    assert(nisyros_queue_get(device, family, 0, &queue) == NISYROS_SUCCESS);

    nisyros_image image = {
        .header = {sizeof(image), NISYROS_ABI_VERSION, NULL},
        .format = NISYROS_IMAGE_VULKAN_PIPELINE_BINARY,
        .byte_size = (uint64_t)size,
        .bytes = package,
    };
    nisyros_executable executable = NULL;
    image.byte_size--;
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_INCOMPATIBLE_BINARY);
    assert(executable == NULL);
    image.byte_size++;
    uint8_t saved_checksum[8];
    memcpy(saved_checksum, package + 32, sizeof(saved_checksum));
    package[44] ^= 1;
    uint64_t changed_checksum = checksum64(package + 40, (size_t)size - 40);
    for (int i = 0; i < 8; ++i) package[32 + i] = (uint8_t)(changed_checksum >> (8 * i));
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_INCOMPATIBLE_BINARY);
    assert(executable == NULL);
    package[44] ^= 1;
    memcpy(package + 32, saved_checksum, sizeof(saved_checksum));
    assert(size > 164);
    package[156] = 1;
    changed_checksum = checksum64(package + 40, (size_t)size - 40);
    for (int i = 0; i < 8; ++i) package[32 + i] = (uint8_t)(changed_checksum >> (8 * i));
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_UNSUPPORTED);
    assert(executable == NULL);
    package[156] = 0;
    memcpy(package + 32, saved_checksum, sizeof(saved_checksum));
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_SUCCESS);
    free(package);
    nisyros_kernel kernel = NULL;
    assert(nisyros_kernel_lookup(executable, "main", &kernel) == NISYROS_SUCCESS);

    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = 64 * sizeof(uint32_t),
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    nisyros_addr source = 0, destination = 0, arg_address = 0;
    assert(nisyros_alloc(device, &allocation, &source) == NISYROS_SUCCESS);
    assert(nisyros_alloc(device, &allocation, &destination) == NISYROS_SUCCESS);
    allocation.bytes = sizeof(arguments) + 8;
    allocation.alignment = 8;
    assert(nisyros_alloc(device, &allocation, &arg_address) == NISYROS_SUCCESS);
    uint32_t *source_host = NULL, *destination_host = NULL;
    void *arg_host = NULL;
    assert(nisyros_map(device, source, 256, (void **)&source_host) == NISYROS_SUCCESS);
    assert(nisyros_map(device, destination, 256, (void **)&destination_host) == NISYROS_SUCCESS);
    assert(nisyros_map(device, arg_address, sizeof(arguments), &arg_host) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) { source_host[i] = i * 3; destination_host[i] = 0; }
    arguments args = {destination, source, 64, 0};
    memcpy(arg_host, &args, sizeof(args));
    assert(nisyros_flush(device, source, 256) == NISYROS_SUCCESS);
    assert(nisyros_flush(device, destination, 256) == NISYROS_SUCCESS);
    assert(nisyros_flush(device, arg_address, sizeof(args)) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, arg_host) == NISYROS_SUCCESS);

    nisyros_cmd cmd = NULL;
    assert(nisyros_cmd_create(device, family, &cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
    nisyros_grid grid = {1, 1, 1};
    assert(nisyros_cmd_dispatch_args(cmd, kernel, &grid, arg_address + 1) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_end(cmd) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_reset(cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch_args(cmd, kernel, &grid, arg_address) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(cmd) == NISYROS_SUCCESS);
    assert(nisyros_executable_destroy(executable) == NISYROS_BUSY);
    nisyros_point done = {0};
    assert(nisyros_submit(queue, cmd, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) assert(destination_host[i] == source_host[i] + 1);
    printf("%s: 64 values verified through native binary\n", info.name);

    assert(nisyros_cmd_destroy(cmd) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) destination_host[i] = 0;
    assert(nisyros_flush(device, destination, 256) == NISYROS_SUCCESS);
    nisyros_cmd inline_cmd = NULL;
    assert(nisyros_cmd_create(device, family, &inline_cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(inline_cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch(inline_cmd, kernel, &grid, &args, sizeof(args) - 1) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_end(inline_cmd) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_reset(inline_cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(inline_cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch(inline_cmd, kernel, &grid, &args, sizeof(args)) == NISYROS_SUCCESS);
    memset(&args, 0, sizeof(args));
    assert(nisyros_cmd_end(inline_cmd) == NISYROS_SUCCESS);
    nisyros_point inline_done = {0};
    assert(nisyros_submit(queue, inline_cmd, NULL, &inline_done) == NISYROS_SUCCESS);
    assert(nisyros_wait(inline_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) assert(destination_host[i] == source_host[i] + 1);
    assert(nisyros_cmd_destroy(inline_cmd) == NISYROS_SUCCESS);

    allocation.bytes = 12;
    allocation.alignment = 4;
    allocation.required_properties = 0;
    allocation.preferred_properties = NISYROS_MEMORY_DEVICE_LOCAL;
    nisyros_addr indirect_grid = 0;
    assert(nisyros_alloc(device, &allocation, &indirect_grid) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) destination_host[i] = 0;
    assert(nisyros_flush(device, destination, 256) == NISYROS_SUCCESS);
    nisyros_cmd indirect = NULL;
    assert(nisyros_cmd_create(device, family, &indirect) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(indirect) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 3; ++i) {
        assert(nisyros_cmd_fill(indirect, indirect_grid + 4 * i, 4, 1) == NISYROS_SUCCESS);
    }
    assert(nisyros_cmd_barrier_range(indirect, indirect_grid, 12) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch_indirect_args(indirect, kernel, indirect_grid, arg_address + 1) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_end(indirect) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_reset(indirect) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(indirect) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 3; ++i) {
        assert(nisyros_cmd_fill(indirect, indirect_grid + 4 * i, 4, 1) == NISYROS_SUCCESS);
    }
    assert(nisyros_cmd_barrier_range(indirect, indirect_grid, 12) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch_indirect_args(indirect, kernel, indirect_grid, arg_address) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(indirect) == NISYROS_SUCCESS);
    nisyros_point indirect_done = {0};
    assert(nisyros_submit(queue, indirect, NULL, &indirect_done) == NISYROS_SUCCESS);
    assert(nisyros_wait(indirect_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) assert(destination_host[i] == source_host[i] + 1);
    assert(nisyros_cmd_destroy(indirect) == NISYROS_SUCCESS);
    assert(nisyros_free(device, indirect_grid) == NISYROS_SUCCESS);
    assert(nisyros_executable_destroy(executable) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, source_host) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, destination_host) == NISYROS_SUCCESS);
    assert(nisyros_free(device, arg_address) == NISYROS_SUCCESS);
    assert(nisyros_free(device, source) == NISYROS_SUCCESS);
    assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    return 0;
}
