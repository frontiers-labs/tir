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

int main(int argc, char **argv) {
    assert(argc == 2);
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count > 0 && count <= 32);
    nisyros_device_id ids[32];
    assert(nisyros_cluster_devices(cluster, &count, ids) == NISYROS_SUCCESS);
    uint8_t uuids[32][16] = {{0}};
    for (uint32_t i = 0; i < count; ++i) {
        assert(ids[i].backend == NISYROS_BACKEND_ROCM);
        nisyros_device_info identity = NISYROS_DESC_INIT(nisyros_device_info);
        assert(nisyros_cluster_device_info(cluster, ids[i], &identity) == NISYROS_SUCCESS);
        memcpy(uuids[i], identity.device_uuid, 16);
        for (uint32_t j = 0; j < i; ++j)
            assert(memcmp(uuids[i], uuids[j], 16) != 0);
    }
    nisyros_device_id id = ids[0];
    nisyros_device_info info = NISYROS_DESC_INIT(nisyros_device_info);
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    assert(info.strict_profile_supported);

    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info.queue_family_count; ++i) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, i, &queue_info) == NISYROS_SUCCESS);
        if ((queue_info.flags & NISYROS_QUEUE_COMPUTE) && queue_info.count) {
            family = i;
            break;
        }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 2, 1.0f, 0};
    nisyros_device_open_desc open = NISYROS_DESC_INIT(nisyros_device_open_desc);
    open.queue_request_count = 1;
    open.queue_requests = &request;
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &open, &device) == NISYROS_SUCCESS);
    nisyros_queue queue = NULL;
    nisyros_queue second_queue = NULL;
    assert(nisyros_queue_get(device, family, 0, &queue) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(device, family, 1, &second_queue) == NISYROS_SUCCESS);

    nisyros_alloc_desc alloc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    alloc.bytes = 64 * sizeof(uint32_t);
    alloc.alignment = 64;
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    alloc.preferred_properties = NISYROS_MEMORY_HOST_COHERENT;
    nisyros_addr source = 0, destination = 0, arg_address = 0;
    assert(nisyros_alloc(device, &alloc, &source) == NISYROS_SUCCESS);
    assert(nisyros_alloc(device, &alloc, &destination) == NISYROS_SUCCESS);
    alloc.bytes = sizeof(arguments);
    alloc.alignment = 8;
    assert(nisyros_alloc(device, &alloc, &arg_address) == NISYROS_SUCCESS);
    uint32_t *source_host = NULL, *destination_host = NULL;
    arguments *arg_host = NULL;
    assert(nisyros_map(device, source, 256, (void **)&source_host) == NISYROS_SUCCESS);
    assert(nisyros_map(device, destination, 256, (void **)&destination_host) == NISYROS_SUCCESS);
    assert(nisyros_map(device, arg_address, sizeof(arguments), (void **)&arg_host) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) {
        source_host[i] = i * 3;
        destination_host[i] = 0;
    }
    *arg_host = (arguments){destination, source, 64, 0};
    assert(nisyros_flush(device, source, 256) == NISYROS_SUCCESS);
    assert(nisyros_flush(device, destination, 256) == NISYROS_SUCCESS);
    assert(nisyros_flush(device, arg_address, sizeof(arguments)) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, arg_host) == NISYROS_SUCCESS);

    FILE *file = fopen(argv[1], "rb");
    assert(file);
    assert(fseek(file, 0, SEEK_END) == 0);
    long size = ftell(file);
    assert(size > 0 && fseek(file, 0, SEEK_SET) == 0);
    void *bytes = malloc((size_t)size);
    assert(bytes && fread(bytes, 1, (size_t)size, file) == (size_t)size);
    assert(fclose(file) == 0);
    nisyros_image image = NISYROS_DESC_INIT(nisyros_image);
    image.format = NISYROS_IMAGE_ROCM_CODE_OBJECT;
    image.byte_size = (uint64_t)size;
    image.bytes = bytes;
    nisyros_executable executable = NULL;
    assert(nisyros_executable_load(device, &image, &executable) == NISYROS_SUCCESS);
    free(bytes);
    nisyros_kernel kernel = NULL;
    assert(nisyros_kernel_lookup(executable, "nisyros_add_one", &kernel) == NISYROS_SUCCESS);

    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(device, family, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    nisyros_grid grid = {1, 1, 1};
    assert(nisyros_cmd_dispatch_args(command, kernel, &grid, arg_address) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit(queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i)
        assert(destination_host[i] == source_host[i] + 1);

    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) destination_host[i] = 0;
    assert(nisyros_flush(device, destination, 256) == NISYROS_SUCCESS);
    arguments inline_args = {destination, source, 64, 0};
    nisyros_cmd inline_command = NULL;
    assert(nisyros_cmd_create(device, family, &inline_command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(inline_command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch(inline_command, kernel, &grid, &inline_args, sizeof(inline_args)) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(inline_command) == NISYROS_SUCCESS);
    assert(nisyros_submit(queue, inline_command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i)
        assert(destination_host[i] == source_host[i] + 1);
    assert(nisyros_cmd_destroy(inline_command) == NISYROS_SUCCESS);

    alloc.bytes = 12;
    alloc.alignment = 4;
    alloc.required_properties = NISYROS_MEMORY_DEVICE_LOCAL;
    alloc.preferred_properties = 0;
    nisyros_addr indirect_grid = 0;
    assert(nisyros_alloc(device, &alloc, &indirect_grid) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i) destination_host[i] = 0;
    assert(nisyros_flush(device, destination, 256) == NISYROS_SUCCESS);
    nisyros_cmd indirect = NULL;
    assert(nisyros_cmd_create(device, family, &indirect) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(indirect) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 3; ++i)
        assert(nisyros_cmd_fill(indirect, indirect_grid + 4 * i, 4, 1) == NISYROS_SUCCESS);
    assert(nisyros_cmd_barrier_range(indirect, indirect_grid, 12) == NISYROS_SUCCESS);
    assert(nisyros_cmd_dispatch_indirect_args(indirect, kernel, indirect_grid, arg_address) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(indirect) == NISYROS_SUCCESS);
    assert(nisyros_submit(queue, indirect, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i)
        assert(destination_host[i] == source_host[i] + 1);
    assert(nisyros_cmd_destroy(indirect) == NISYROS_SUCCESS);
    assert(nisyros_free(device, indirect_grid) == NISYROS_SUCCESS);

    alloc.bytes = 16;
    alloc.alignment = 8;
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    nisyros_addr timestamps = 0;
    assert(nisyros_alloc(device, &alloc, &timestamps) == NISYROS_SUCCESS);
    uint64_t *times = NULL;
    assert(nisyros_map(device, timestamps, 16, (void **)&times) == NISYROS_SUCCESS);
    nisyros_cmd timing = NULL;
    assert(nisyros_cmd_create(device, family, &timing) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(timing) == NISYROS_SUCCESS);
    assert(nisyros_cmd_write_timestamp(timing, timestamps) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(timing, destination, 256, 0) == NISYROS_SUCCESS);
    assert(nisyros_cmd_write_timestamp(timing, timestamps + 8) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(timing) == NISYROS_SUCCESS);
    assert(nisyros_submit(queue, timing, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, timestamps, 16) == NISYROS_SUCCESS);
    assert(times[1] >= times[0]);
    assert(nisyros_cmd_destroy(timing) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, times) == NISYROS_SUCCESS);
    assert(nisyros_free(device, timestamps) == NISYROS_SUCCESS);

    nisyros_event_desc event_desc = NISYROS_DESC_INIT(nisyros_event_desc);
    event_desc.signaling_domain = NISYROS_SIGNAL_DEVICE;
    nisyros_event staged = NULL;
    assert(nisyros_event_create(device, &event_desc, &staged) == NISYROS_SUCCESS);
    nisyros_point ready = {staged, 1};
    nisyros_cmd consumer = NULL, producer = NULL;
    assert(nisyros_cmd_create(device, family, &consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_wait(consumer, ready) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy(consumer, destination, source, 256) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_create(device, family, &producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(producer, source, 256, 0x10203040) == NISYROS_SUCCESS);
    assert(nisyros_cmd_signal(producer, ready) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
    nisyros_point consumed = {0};
    assert(nisyros_submit(second_queue, consumer, NULL, &consumed) == NISYROS_SUCCESS);
    assert(nisyros_wait(consumed, 0) == NISYROS_TIMEOUT);
    assert(nisyros_submit(queue, producer, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(consumed, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 256) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < 64; ++i)
        assert(destination_host[i] == 0x10203040);
    assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(staged) == NISYROS_SUCCESS);

    nisyros_host_alloc_desc host_desc = NISYROS_DESC_INIT(nisyros_host_alloc_desc);
    host_desc.bytes = 64;
    host_desc.alignment = 64;
    void *upload = NULL, *download = NULL;
    assert(nisyros_host_alloc(device, &host_desc, &upload) == NISYROS_SUCCESS);
    assert(nisyros_host_alloc(device, &host_desc, &download) == NISYROS_SUCCESS);
    memset(upload, 0x5a, 64);
    memset(download, 0, 64);
    nisyros_cmd transfer = NULL;
    assert(nisyros_cmd_create(device, family, &transfer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(transfer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy_h2d(transfer, source, (unsigned char *)upload + 4, 16) == NISYROS_SUCCESS);
    assert(nisyros_cmd_barrier_all(transfer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy_d2h(transfer, (unsigned char *)download + 8, source, 16) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(transfer) == NISYROS_SUCCESS);
    assert(nisyros_submit(queue, transfer, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    for (int i = 8; i < 24; ++i) assert(((unsigned char *)download)[i] == 0x5a);
    assert(nisyros_cmd_destroy(transfer) == NISYROS_SUCCESS);
    assert(nisyros_host_free(device, upload) == NISYROS_SUCCESS);
    assert(nisyros_host_free(device, download) == NISYROS_SUCCESS);

    uint64_t registration_alignment = 0;
    assert(nisyros_host_registration_alignment(device, &registration_alignment) == NISYROS_SUCCESS);
    void *registered_upload = aligned_alloc((size_t)registration_alignment, (size_t)registration_alignment);
    void *registered_download = aligned_alloc((size_t)registration_alignment, (size_t)registration_alignment);
    assert(registered_upload && registered_download);
    memset(registered_upload, 0x69, (size_t)registration_alignment);
    memset(registered_download, 0, (size_t)registration_alignment);
    assert(nisyros_host_register(device, registered_upload, registration_alignment) == NISYROS_SUCCESS);
    assert(nisyros_host_register(device, registered_download, registration_alignment) == NISYROS_SUCCESS);
    nisyros_cmd registered_transfer = NULL;
    assert(nisyros_cmd_create(device, family, &registered_transfer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(registered_transfer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy_h2d(registered_transfer, source, registered_upload, 16) == NISYROS_SUCCESS);
    assert(nisyros_cmd_barrier_all(registered_transfer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy_d2h(registered_transfer, registered_download, source, 16) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(registered_transfer) == NISYROS_SUCCESS);
    assert(nisyros_submit(queue, registered_transfer, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    for (int i = 0; i < 16; ++i) assert(((unsigned char *)registered_download)[i] == 0x69);
    assert(nisyros_cmd_destroy(registered_transfer) == NISYROS_SUCCESS);
    assert(nisyros_host_unregister(device, registered_upload) == NISYROS_SUCCESS);
    assert(nisyros_host_unregister(device, registered_download) == NISYROS_SUCCESS);
    free(registered_upload);
    free(registered_download);

    assert(nisyros_executable_destroy(executable) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, source_host) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, destination_host) == NISYROS_SUCCESS);
    assert(nisyros_free(device, arg_address) == NISYROS_SUCCESS);
    assert(nisyros_free(device, source) == NISYROS_SUCCESS);
    assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("ROCm native code object: 64 values verified");
    return 0;
}
