#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS && count == 1);
    nisyros_device_id id;
    assert(nisyros_cluster_devices(cluster, &count, &id) == NISYROS_SUCCESS);
    assert(id.backend == 0x80000001u);
    nisyros_device_info info = NISYROS_DESC_INIT(nisyros_device_info);
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    assert(info.backend == id.backend && info.strict_profile_supported);

    nisyros_queue_request request = {0, 1, 1.0f, 0};
    nisyros_device_open_desc desc = NISYROS_DESC_INIT(nisyros_device_open_desc);
    desc.queue_request_count = 1;
    desc.queue_requests = &request;
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &desc, &device) == NISYROS_SUCCESS);
    nisyros_queue queue = NULL;
    assert(nisyros_queue_get(device, 0, 0, &queue) == NISYROS_SUCCESS);

    nisyros_alloc_desc alloc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    alloc.bytes = 64;
    alloc.alignment = 64;
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    nisyros_addr source = 0, destination = 0;
    assert(nisyros_alloc(device, &alloc, &source) == NISYROS_SUCCESS);
    assert(nisyros_alloc(device, &alloc, &destination) == NISYROS_SUCCESS);

    nisyros_cmd cmd = NULL;
    assert(nisyros_cmd_create(device, 0, &cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(cmd, source, 64, 0xa1b2c3d4u) == NISYROS_SUCCESS);
    assert(nisyros_cmd_barrier_all(cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy(cmd, destination, source, 64) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(cmd) == NISYROS_SUCCESS);
    nisyros_point completion = {0};
    assert(nisyros_submit(queue, cmd, NULL, &completion) == NISYROS_SUCCESS);
    assert(nisyros_wait(completion, 1000000000ull) == NISYROS_SUCCESS);

    void *host = NULL;
    assert(nisyros_map(device, destination, 64, &host) == NISYROS_SUCCESS);
    assert(host != NULL);
    for (int i = 0; i < 16; ++i) assert(((uint32_t *)host)[i] == 0xa1b2c3d4u);
    assert(nisyros_unmap(device, host) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(cmd) == NISYROS_SUCCESS);
    assert(nisyros_free(device, source) == NISYROS_SUCCESS);
    assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("independent provider completed fill, copy, and timeline submission");
    return 0;
}
