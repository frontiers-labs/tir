#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    assert(argc == 5);
    uint32_t local_node = (uint32_t)strtoul(argv[1], NULL, 10);
    uint64_t session_id = strtoull(argv[2], NULL, 10);
    uint32_t expected_local = (uint32_t)strtoul(argv[4], NULL, 10);
    nisyros_node_address nodes[2] = {{0, 0, argv[3]}, {1, 0, NULL}};
    nisyros_bootstrap_desc bootstrap = {
        .header = {sizeof(bootstrap), NISYROS_ABI_VERSION, NULL},
        .session_id = session_id,
        .local_node = local_node,
        .node_count = 2,
        .nodes = nodes,
        .connect_timeout_ns = UINT64_C(10000000000),
    };
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_join(&bootstrap, &cluster) == NISYROS_SUCCESS);
    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count <= 32);
    nisyros_device_id ids[32];
    uint32_t capacity = 32;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_SUCCESS);
    assert(capacity == count);
    uint32_t local_seen = 0;
    for (uint32_t index = 0; index < count; ++index) {
        nisyros_device_info info = {
            .header = {sizeof(info), NISYROS_ABI_VERSION, NULL},
        };
        assert(nisyros_cluster_device_info(cluster, ids[index], &info) == NISYROS_SUCCESS);
        assert(info.node_index <= 1);
        assert(info.is_local == (info.node_index == local_node));
        for (uint32_t family = 0; family < info.queue_family_count; ++family) {
            nisyros_queue_family_info queue_info;
            assert(nisyros_cluster_queue_family_info(cluster, ids[index], family,
                                                     &queue_info) == NISYROS_SUCCESS);
            if (queue_info.timestamp_valid_bits) {
                assert(queue_info.timestamp_period_ns > 0.0f);
                assert(queue_info.timestamp_domain == NISYROS_CLOCK_DEVICE);
            }
        }
        uint32_t heap_count = 0;
        assert(nisyros_cluster_memory_heaps(cluster, ids[index], &heap_count, NULL) == NISYROS_SUCCESS);
        assert(heap_count > 0 && heap_count <= 16);
        nisyros_device_capabilities caps = {
            .header = {sizeof(caps), NISYROS_ABI_VERSION, NULL},
        };
        assert(nisyros_cluster_device_capabilities(cluster, ids[index], &caps) == NISYROS_SUCCESS);
        uint32_t tuple_count = 0;
        assert(nisyros_cluster_cooperative_matrices(cluster, ids[index], &tuple_count, NULL) == NISYROS_SUCCESS);
        assert((caps.features & NISYROS_FEATURE_COOPERATIVE_MATRIX) != 0 ? tuple_count > 0 : tuple_count == 0);
        if (info.is_local) local_seen++;
        else {
            nisyros_queue_request request = {0, 1, 1.0f, 0};
            nisyros_device_open_desc open = {
                .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
                .queue_request_count = 1,
                .queue_requests = &request,
            };
            nisyros_device refused = NULL;
            assert(nisyros_device_open(cluster, ids[index], &open, &refused) == NISYROS_UNSUPPORTED);
            assert(refused == NULL);
        }
        printf("%llu:%u:%u:%u:%u:", (unsigned long long)ids[index].epoch,
               ids[index].index, ids[index].backend, info.node_index, info.is_local);
        for (int byte = 0; byte < 16; ++byte) printf("%02x", info.device_uuid[byte]);
        putchar('\n');
    }
    assert(local_seen == expected_local);
    nisyros_group_desc group_desc = {
        .header = {sizeof(group_desc), NISYROS_ABI_VERSION, NULL},
        .group_epoch = session_id,
        .member_count = count,
        .members = ids,
    };
    nisyros_group group = NULL;
    assert(nisyros_group_create(cluster, &group_desc, &group) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_BUSY);
    uint32_t ranks = 0;
    assert(nisyros_group_rank_count(group, &ranks) == NISYROS_SUCCESS);
    assert(ranks == count);
    uint64_t group_epoch = 0;
    assert(nisyros_group_epoch(group, &group_epoch) == NISYROS_SUCCESS);
    assert(group_epoch == session_id);
    for (uint32_t rank = 0; rank < ranks; ++rank) {
        nisyros_device_id member;
        assert(nisyros_group_member(group, rank, &member) == NISYROS_SUCCESS);
        assert(member.epoch == ids[rank].epoch && member.index == ids[rank].index);
    }
    assert(nisyros_group_destroy(group) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    return 0;
}
