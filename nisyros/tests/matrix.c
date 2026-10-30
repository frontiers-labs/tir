#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint64_t checksum64(const uint8_t *bytes, size_t size) {
    uint64_t hash = UINT64_C(0xcbf29ce484222325);
    for (size_t i = 0; i < size; ++i) hash = (hash ^ bytes[i]) * UINT64_C(0x100000001b3);
    return hash;
}

static void update_checksum(uint8_t *bytes, size_t size) {
    uint64_t checksum = checksum64(bytes + 40, size - 40);
    for (int i = 0; i < 8; ++i) bytes[32 + i] = (uint8_t)(checksum >> (8 * i));
}

int main(int argc, char **argv) {
    assert(argc == 2);
    int probe = strcmp(argv[1], "--probe") == 0;

    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 1;
    nisyros_device_id id;
    assert(nisyros_cluster_devices(cluster, &count, &id) == NISYROS_SUCCESS);
    assert(count == 1);
    nisyros_device_capabilities caps = {
        .header = {sizeof(caps), NISYROS_ABI_VERSION, NULL},
    };
    assert(nisyros_cluster_device_capabilities(cluster, id, &caps) == NISYROS_SUCCESS);
    uint64_t required = NISYROS_FEATURE_COOPERATIVE_MATRIX |
                        NISYROS_FEATURE_SHADER_FLOAT16 |
                        NISYROS_FEATURE_VULKAN_MEMORY_MODEL;
    if ((caps.features & required) != required || caps.cooperative_matrix_revision < 2) {
        printf("cooperative matrix requirements unavailable\n");
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        return 77;
    }
    uint32_t tuple_count = 256;
    nisyros_cooperative_matrix_tuple tuples[256];
    assert(nisyros_cluster_cooperative_matrices(cluster, id, &tuple_count, tuples) == NISYROS_SUCCESS);
    int found = 0;
    for (uint32_t i = 0; i < tuple_count; ++i) {
        const nisyros_cooperative_matrix_tuple *tuple = &tuples[i];
        if (tuple->m == 16 && tuple->n == 16 && tuple->k == 16 &&
            tuple->a_type == NISYROS_COMPONENT_FLOAT16 &&
            tuple->b_type == NISYROS_COMPONENT_FLOAT16 &&
            tuple->c_type == NISYROS_COMPONENT_FLOAT32 &&
            tuple->result_type == NISYROS_COMPONENT_FLOAT32 &&
            tuple->saturating_accumulation == 0 && tuple->scope == NISYROS_SCOPE_SUBGROUP) found = 1;
    }
    if (!found) {
        printf("FP16-to-FP32 matrix tuple unavailable\n");
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        return 77;
    }
    if (probe) {
        printf("FP16-to-FP32 matrix tuple available\n");
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        return 0;
    }

    FILE *file = fopen(argv[1], "rb");
    assert(file != NULL);
    assert(fseek(file, 0, SEEK_END) == 0);
    long size = ftell(file);
    assert(size > 200);
    assert(fseek(file, 0, SEEK_SET) == 0);
    uint8_t *package = malloc((size_t)size);
    assert(package != NULL);
    assert(fread(package, 1, (size_t)size, file) == (size_t)size);
    assert(fclose(file) == 0);
    assert(package[8] == 2 && package[9] == 0 && package[10] == 0 && package[11] == 0);
    assert(package[132] == (NISYROS_FEATURE_COOPERATIVE_MATRIX |
                            NISYROS_FEATURE_SHADER_FLOAT16 |
                            NISYROS_FEATURE_VULKAN_MEMORY_MODEL));

    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info.queue_family_count; ++i) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, i, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_COMPUTE) { family = i; break; }
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
    package[132] |= NISYROS_FEATURE_BFLOAT16_TYPE;
    update_checksum(package, (size_t)size);
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_UNSUPPORTED);
    assert(executable == NULL);
    package[132] &= (uint8_t)~NISYROS_FEATURE_BFLOAT16_TYPE;
    package[160] = 8;
    update_checksum(package, (size_t)size);
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_UNSUPPORTED);
    assert(executable == NULL);
    package[160] = 16;
    update_checksum(package, (size_t)size);
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_SUCCESS);
    free(package);
    nisyros_kernel kernel = NULL;
    assert(nisyros_kernel_lookup(executable, "main", &kernel) == NISYROS_SUCCESS);

    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = 512 * sizeof(float),
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    nisyros_addr address = 0;
    assert(nisyros_alloc(device, &allocation, &address) == NISYROS_SUCCESS);
    float *output = NULL;
    assert(nisyros_map(device, address, allocation.bytes, (void **)&output) == NISYROS_SUCCESS);
    for (int i = 0; i < 512; ++i) output[i] = -1.0f;
    assert(nisyros_flush(device, address, allocation.bytes) == NISYROS_SUCCESS);
    nisyros_cmd cmd = NULL;
    assert(nisyros_cmd_create(device, family, &cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
    nisyros_grid grid = {1, 1, 1};
    assert(nisyros_cmd_dispatch(cmd, kernel, &grid, &address, sizeof(address)) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(cmd) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit(queue, cmd, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, address, allocation.bytes) == NISYROS_SUCCESS);
    for (int i = 0; i < 256; ++i) assert(output[i] == 97.0f);
    printf("%s: 16x16x16 cooperative matrix verified through native binary\n", info.name);
    assert(nisyros_cmd_destroy(cmd) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, output) == NISYROS_SUCCESS);
    assert(nisyros_free(device, address) == NISYROS_SUCCESS);
    assert(nisyros_executable_destroy(executable) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    return 0;
}
