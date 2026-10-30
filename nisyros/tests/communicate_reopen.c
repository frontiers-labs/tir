#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    assert(argc == 4);
    uint32_t node = (uint32_t)strtoul(argv[1], NULL, 10);
    uint64_t session = strtoull(argv[2], NULL, 10);
    nisyros_node_address nodes[2] = {{0, 0, argv[3]}, {1, 0, NULL}};
    nisyros_bootstrap_desc bootstrap = NISYROS_DESC_INIT(nisyros_bootstrap_desc);
    bootstrap.session_id = session;
    bootstrap.local_node = node;
    bootstrap.node_count = 2;
    bootstrap.nodes = nodes;
    bootstrap.connect_timeout_ns = UINT64_C(10000000000);
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_join(&bootstrap, &cluster) == NISYROS_SUCCESS);
    nisyros_device_id ids[2];
    uint32_t count = 2;
    assert(nisyros_cluster_devices(cluster, &count, ids) == NISYROS_SUCCESS && count == 2);
    nisyros_device_info info = NISYROS_DESC_INIT(nisyros_device_info);
    assert(nisyros_cluster_device_info(cluster, ids[node], &info) == NISYROS_SUCCESS);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info.queue_family_count; ++i) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, ids[node], i, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_TRANSFER) { family = i; break; }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = NISYROS_DESC_INIT(nisyros_device_open_desc);
    open.queue_request_count = 1;
    open.queue_requests = &request;
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, ids[node], &open, &device) == NISYROS_SUCCESS);
    nisyros_queue queue = NULL;
    assert(nisyros_queue_get(device, family, 0, &queue) == NISYROS_SUCCESS);
    nisyros_alloc_desc alloc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    alloc.bytes = 64;
    alloc.alignment = 64;
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    nisyros_addr address = 0;
    assert(nisyros_alloc(device, &alloc, &address) == NISYROS_SUCCESS);
    void *host = NULL;
    assert(nisyros_map(device, address, 64, &host) == NISYROS_SUCCESS);

    for (uint32_t phase = 0; phase < 2; ++phase) {
        memset(host, node == 0 ? 0x44 + phase : 0, 64);
        assert(nisyros_flush(device, address, 64) == NISYROS_SUCCESS);
        nisyros_group_desc group_desc = NISYROS_DESC_INIT(nisyros_group_desc);
        group_desc.group_epoch = session + phase;
        group_desc.member_count = 2;
        group_desc.members = ids;
        nisyros_group group = NULL;
        assert(nisyros_group_create(cluster, &group_desc, &group) == NISYROS_SUCCESS);
        nisyros_rank_binding binding = {node, 0, device, queue};
        nisyros_endpoint_desc endpoint_desc = NISYROS_DESC_INIT(nisyros_endpoint_desc);
        endpoint_desc.binding_count = 1;
        endpoint_desc.bindings = &binding;
        endpoint_desc.allow_host_staging = 1;
        endpoint_desc.failure_timeout_ns = UINT64_C(5000000000);
        nisyros_endpoint endpoint = NULL;
        assert(nisyros_endpoint_open(group, &endpoint_desc, &endpoint) == NISYROS_SUCCESS);
        nisyros_comm_cmd cmd = NULL;
        assert(nisyros_comm_cmd_create(endpoint, &cmd) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_begin(cmd) == NISYROS_SUCCESS);
        if (node == 0)
            assert(nisyros_comm_cmd_send(cmd, 0, 1, 1, 1, address, 64) == NISYROS_SUCCESS);
        else
            assert(nisyros_comm_cmd_recv(cmd, 1, 0, 1, 1, address, 64) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_end(cmd) == NISYROS_SUCCESS);
        nisyros_point done = {0};
        assert(nisyros_endpoint_submit(endpoint, cmd, NULL, &done) == NISYROS_SUCCESS);
        assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        if (node == 1) {
            assert(nisyros_invalidate(device, address, 64) == NISYROS_SUCCESS);
            for (int i = 0; i < 64; ++i) assert(((unsigned char *)host)[i] == 0x44 + phase);
        }
        assert(nisyros_comm_cmd_destroy(cmd) == NISYROS_SUCCESS);
        assert(nisyros_endpoint_close(endpoint) == NISYROS_SUCCESS);
        assert(nisyros_group_destroy(group) == NISYROS_SUCCESS);
        printf("node %u closed phase %u\n", node, phase);
        fflush(stdout);
        assert(getchar() == '\n');
    }
    assert(nisyros_unmap(device, host) == NISYROS_SUCCESS);
    assert(nisyros_free(device, address) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    return 0;
}
