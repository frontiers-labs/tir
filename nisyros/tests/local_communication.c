#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef struct {
    nisyros_device device;
    nisyros_queue queue;
    nisyros_addr address;
    void *host;
} local_device;

static void open_device(nisyros_cluster cluster, nisyros_device_id id,
                        local_device *out) {
    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    assert(info.is_local == 1 && info.strict_profile_supported == 1);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info.queue_family_count; ++i) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, i, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_TRANSFER) { family = i; break; }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    assert(nisyros_device_open(cluster, id, &open, &out->device) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(out->device, family, 0, &out->queue) == NISYROS_SUCCESS);
    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = 64,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    assert(nisyros_alloc(out->device, &allocation, &out->address) == NISYROS_SUCCESS);
    assert(nisyros_map(out->device, out->address, 64, &out->host) == NISYROS_SUCCESS);
}

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 2;
    nisyros_device_id ids[2];
    assert(nisyros_cluster_devices(cluster, &count, ids) == NISYROS_SUCCESS);
    assert(count == 2);
    nisyros_group_desc group_desc = {
        .header = {sizeof(group_desc), NISYROS_ABI_VERSION, NULL},
        .group_epoch = 1,
        .member_count = 2,
        .members = ids,
    };
    nisyros_group group = NULL;
    assert(nisyros_group_create(cluster, &group_desc, &group) == NISYROS_SUCCESS);
    local_device local[2] = {0};
    for (int i = 0; i < 2; ++i) open_device(cluster, ids[i], &local[i]);
    memset(local[0].host, 0x67, 64);
    memset(local[1].host, 0, 64);
    assert(nisyros_flush(local[0].device, local[0].address, 64) == NISYROS_SUCCESS);
    assert(nisyros_flush(local[1].device, local[1].address, 64) == NISYROS_SUCCESS);
    nisyros_rank_binding bindings[2] = {
        {0, 0, local[0].device, local[0].queue},
        {1, 0, local[1].device, local[1].queue},
    };
    nisyros_endpoint_desc endpoint_desc = {
        .header = {sizeof(endpoint_desc), NISYROS_ABI_VERSION, NULL},
        .binding_count = 2,
        .bindings = bindings,
        .allow_host_staging = 1,
        .failure_timeout_ns = UINT64_C(10000000000),
    };
    nisyros_endpoint endpoint = NULL;
    assert(nisyros_endpoint_open(group, &endpoint_desc, &endpoint) == NISYROS_SUCCESS);
    nisyros_endpoint_info endpoint_info = {.header = {sizeof(endpoint_info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_endpoint_query(endpoint, &endpoint_info) == NISYROS_SUCCESS);
    assert(endpoint_info.worker_threads == 1 && endpoint_info.uses_host_staging == 1);
    nisyros_comm_cmd command = NULL;
    assert(nisyros_comm_cmd_create(endpoint, &command) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_send(command, 0, 1, 3, 1, local[0].address, 64) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_recv(command, 1, 0, 3, 1, local[1].address, 64) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_endpoint_submit(endpoint, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(local[1].device, local[1].address, 64) == NISYROS_SUCCESS);
    for (int i = 0; i < 64; ++i) assert(((unsigned char *)local[1].host)[i] == 0x67);
    assert(nisyros_comm_cmd_destroy(command) == NISYROS_SUCCESS);
    for (int rank = 0; rank < 2; ++rank) {
        for (int word = 0; word < 4; ++word) ((uint32_t *)local[rank].host)[word] = (uint32_t)rank + 1;
        assert(nisyros_flush(local[rank].device, local[rank].address, 16) == NISYROS_SUCCESS);
    }
    nisyros_comm_cmd collective = NULL;
    assert(nisyros_comm_cmd_create(endpoint, &collective) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_begin(collective) == NISYROS_SUCCESS);
    for (uint32_t rank = 0; rank < 2; ++rank) {
        assert(nisyros_comm_cmd_all_reduce(collective, rank, 2, local[rank].address,
                                            local[rank].address, 4, NISYROS_DTYPE_U32,
                                            NISYROS_REDUCE_SUM) == NISYROS_SUCCESS);
    }
    assert(nisyros_comm_cmd_end(collective) == NISYROS_SUCCESS);
    nisyros_point collective_done = {0};
    assert(nisyros_endpoint_submit(endpoint, collective, NULL, &collective_done) == NISYROS_SUCCESS);
    assert(nisyros_wait(collective_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    for (int rank = 0; rank < 2; ++rank) {
        assert(nisyros_invalidate(local[rank].device, local[rank].address, 16) == NISYROS_SUCCESS);
        for (int word = 0; word < 4; ++word) assert(((uint32_t *)local[rank].host)[word] == 3);
    }
    assert(nisyros_comm_cmd_destroy(collective) == NISYROS_SUCCESS);
    nisyros_comm_cmd invalid = NULL;
    assert(nisyros_comm_cmd_create(endpoint, &invalid) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_begin(invalid) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_all_reduce(invalid, 0, 3, local[0].address,
                                        local[0].address, 4, NISYROS_DTYPE_U32,
                                        NISYROS_REDUCE_SUM) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_all_reduce(invalid, 1, 3, local[1].address,
                                        local[1].address, 3, NISYROS_DTYPE_U32,
                                        NISYROS_REDUCE_SUM) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_end(invalid) == NISYROS_SUCCESS);
    nisyros_point rejected = {0};
    assert(nisyros_endpoint_submit(endpoint, invalid, NULL, &rejected) == NISYROS_INVALID_ARGUMENT);
    assert(rejected.event == NULL);
    assert(nisyros_comm_cmd_destroy(invalid) == NISYROS_SUCCESS);
    assert(nisyros_endpoint_close(endpoint) == NISYROS_SUCCESS);
    for (int i = 0; i < 2; ++i) {
        assert(nisyros_unmap(local[i].device, local[i].host) == NISYROS_SUCCESS);
        assert(nisyros_free(local[i].device, local[i].address) == NISYROS_SUCCESS);
        assert(nisyros_device_close(local[i].device) == NISYROS_SUCCESS);
    }
    assert(nisyros_group_destroy(group) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    printf("two local ranks exchanged data and reduced in place without a network worker\n");
    return 0;
}
