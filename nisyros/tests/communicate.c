#define _POSIX_C_SOURCE 200809L
#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef struct {
    uint32_t rank;
    nisyros_device device;
    nisyros_queue queue;
    uint32_t family;
    nisyros_addr address;
    void *host;
    uint64_t bytes;
} local_rank;

static void open_rank(nisyros_cluster cluster, nisyros_group group, uint32_t rank,
                      local_rank *local) {
    nisyros_device_id id;
    assert(nisyros_group_member(group, rank, &id) == NISYROS_SUCCESS);
    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    assert(info.is_local == 1 && info.strict_profile_supported == 1);
    uint32_t family = UINT32_MAX;
    for (uint32_t index = 0; index < info.queue_family_count; ++index) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, index, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_TRANSFER) { family = index; break; }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    local->rank = rank;
    local->family = family;
    assert(nisyros_device_open(cluster, id, &open, &local->device) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(local->device, family, 0, &local->queue) == NISYROS_SUCCESS);
    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = 128,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    local->bytes = allocation.bytes;
    assert(nisyros_alloc(local->device, &allocation, &local->address) == NISYROS_SUCCESS);
    assert(nisyros_map(local->device, local->address, local->bytes, &local->host) == NISYROS_SUCCESS);
    memset(local->host, rank == 0 ? 0x31 : rank == 1 ? 0x42 : 0, (size_t)local->bytes);
    assert(nisyros_flush(local->device, local->address, local->bytes) == NISYROS_SUCCESS);
}

int main(int argc, char **argv) {
    assert(argc == 4 || argc == 5);
    int peer_loss = argc == 5 && strcmp(argv[4], "peer-loss") == 0;
    int mismatch = argc == 5 && strcmp(argv[4], "mismatch") == 0;
    int replay = argc == 5 && strcmp(argv[4], "replay") == 0;
    int collective_mismatch = argc == 5 && strcmp(argv[4], "collective-mismatch") == 0;
    int collective_timeout = argc == 5 && strcmp(argv[4], "collective-timeout") == 0;
    uint32_t node = (uint32_t)strtoul(argv[1], NULL, 10);
    uint64_t session_id = strtoull(argv[2], NULL, 10);
    nisyros_node_address nodes[2] = {{0, 0, argv[3]}, {1, 0, NULL}};
    nisyros_bootstrap_desc bootstrap = {
        .header = {sizeof(bootstrap), NISYROS_ABI_VERSION, NULL},
        .session_id = session_id,
        .local_node = node,
        .node_count = 2,
        .nodes = nodes,
        .connect_timeout_ns = UINT64_C(10000000000),
    };
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_join(&bootstrap, &cluster) == NISYROS_SUCCESS);
    nisyros_device_id ids[3];
    uint32_t count = 3;
    assert(nisyros_cluster_devices(cluster, &count, ids) == NISYROS_SUCCESS);
    assert(count == 3);
    nisyros_group_desc group_desc = {
        .header = {sizeof(group_desc), NISYROS_ABI_VERSION, NULL},
        .group_epoch = session_id,
        .member_count = 3,
        .members = ids,
    };
    nisyros_group group = NULL;
    assert(nisyros_group_create(cluster, &group_desc, &group) == NISYROS_SUCCESS);

    local_rank local[2] = {0};
    uint32_t local_count = node == 0 ? 2 : 1;
    for (uint32_t i = 0; i < local_count; ++i) {
        open_rank(cluster, group, node == 0 ? i : 2, &local[i]);
    }
    nisyros_rank_binding bindings[2] = {0};
    for (uint32_t i = 0; i < local_count; ++i) {
        bindings[i] = (nisyros_rank_binding){local[i].rank, 0, local[i].device, local[i].queue};
    }
    nisyros_endpoint_desc endpoint_desc = {
        .header = {sizeof(endpoint_desc), NISYROS_ABI_VERSION, NULL},
        .binding_count = local_count,
        .bindings = bindings,
        .allow_host_staging = 1,
        .failure_timeout_ns = collective_timeout ? UINT64_C(1000000000) : UINT64_C(10000000000),
    };
    nisyros_endpoint endpoint = NULL;
    assert(nisyros_endpoint_open(group, &endpoint_desc, &endpoint) == NISYROS_SUCCESS);
    nisyros_endpoint_info endpoint_info = {
        .header = {sizeof(endpoint_info), NISYROS_ABI_VERSION, NULL},
    };
    assert(nisyros_endpoint_query(endpoint, &endpoint_info) == NISYROS_SUCCESS);
    assert(endpoint_info.software_progress == 1 && endpoint_info.uses_host_staging == 1);
    if (peer_loss && node == 1) _Exit(0);
    nisyros_event input_ready = NULL;
    nisyros_cmd gpu_producer = NULL;
    if (node == 0 && !peer_loss) {
        nisyros_event_desc ready_desc = {
            .header = {sizeof(ready_desc), NISYROS_ABI_VERSION, NULL},
            .signaling_domain = NISYROS_SIGNAL_DEVICE,
        };
        assert(nisyros_event_create(local[0].device, &ready_desc, &input_ready) == NISYROS_SUCCESS);
        assert(nisyros_cmd_create(local[0].device, local[0].family, &gpu_producer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(gpu_producer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(gpu_producer, local[0].address, 64, 0x31313131) == NISYROS_SUCCESS);
        assert(nisyros_cmd_signal(gpu_producer, (nisyros_point){input_ready, 1}) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(gpu_producer) == NISYROS_SUCCESS);
    }
    if (node == 0 && !peer_loss && !mismatch) {
        nisyros_comm_cmd duplicate = NULL;
        assert(nisyros_comm_cmd_create(endpoint, &duplicate) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_begin(duplicate) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_send(duplicate, 0, 2, 99, 1, local[0].address, 64) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_send(duplicate, 0, 2, 99, 1, local[0].address, 64) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_end(duplicate) == NISYROS_SUCCESS);
        nisyros_point rejected = {0};
        assert(nisyros_endpoint_submit(endpoint, duplicate, NULL, &rejected) == NISYROS_INVALID_ARGUMENT);
        assert(rejected.event == NULL);
        assert(nisyros_comm_cmd_destroy(duplicate) == NISYROS_SUCCESS);
    }

    nisyros_comm_cmd cmd = NULL;
    nisyros_cmd dependent = NULL;
    assert(nisyros_comm_cmd_create(endpoint, &cmd) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_begin(cmd) == NISYROS_SUCCESS);
    if (input_ready != NULL) {
        assert(nisyros_comm_cmd_wait(cmd, (nisyros_point){input_ready, 1}) == NISYROS_SUCCESS);
    }
    if (node == 0) {
        if (replay) {
            assert(nisyros_comm_cmd_send_slot(cmd, 0, 2, 7, 0, local[0].address, 64) == NISYROS_SUCCESS);
            assert(nisyros_comm_cmd_send_slot(cmd, 1, 2, 7, 0, local[1].address, 64) == NISYROS_SUCCESS);
        } else {
            assert(nisyros_comm_cmd_send(cmd, 0, 2, 7, 1, local[0].address, 64) == NISYROS_SUCCESS);
            if (!peer_loss && !mismatch) {
                assert(nisyros_comm_cmd_send(cmd, 1, 2, 7, 1, local[1].address, 64) == NISYROS_SUCCESS);
                assert(nisyros_comm_cmd_send(cmd, 0, 1, 8, 1, local[0].address, 64) == NISYROS_SUCCESS);
                assert(nisyros_comm_cmd_recv(cmd, 1, 0, 8, 1, local[1].address + 64, 64) == NISYROS_SUCCESS);
            }
        }
    } else {
        if (replay) {
            assert(nisyros_comm_cmd_recv_slot(cmd, 2, 0, 7, 0, local[0].address, 64) == NISYROS_SUCCESS);
            assert(nisyros_comm_cmd_recv_slot(cmd, 2, 1, 7, 0, local[0].address + 64, 64) == NISYROS_SUCCESS);
        } else {
            assert(nisyros_comm_cmd_recv(cmd, 2, 0, 7, 1, local[0].address,
                                         mismatch ? 32 : 64) == NISYROS_SUCCESS);
            if (!mismatch) {
                assert(nisyros_comm_cmd_recv(cmd, 2, 1, 7, 1, local[0].address + 64, 64) == NISYROS_SUCCESS);
            }
        }
    }
    assert(nisyros_comm_cmd_end(cmd) == NISYROS_SUCCESS);
    volatile uint64_t host_work = 0;
    if (replay) {
        nisyros_point rejected = {0};
        assert(nisyros_endpoint_submit(endpoint, cmd, NULL, &rejected) == NISYROS_INVALID_ARGUMENT);
        assert(rejected.event == NULL);
        for (uint64_t iteration = 1; iteration <= 5; ++iteration) {
            if (node == 1) {
                memset(local[0].host, 0, 128);
                assert(nisyros_flush(local[0].device, local[0].address, 128) == NISYROS_SUCCESS);
            }
            nisyros_sequence_binding binding = {0, 0, iteration};
            nisyros_comm_submit_desc submit_desc = {
                .header = {sizeof(submit_desc), NISYROS_ABI_VERSION, NULL},
                .binding_count = 1,
                .bindings = &binding,
            };
            nisyros_point replay_done = {0};
            assert(nisyros_endpoint_submit(endpoint, cmd, &submit_desc,
                                           &replay_done) == NISYROS_SUCCESS);
            if (node == 0 && iteration == 1) {
                nisyros_point producer_done = {0};
                assert(nisyros_submit(local[0].queue, gpu_producer, NULL,
                                      &producer_done) == NISYROS_SUCCESS);
            }
            for (uint32_t i = 0; i < 100000; ++i) host_work += i;
            assert(nisyros_wait(replay_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            if (node == 1) {
                assert(nisyros_invalidate(local[0].device, local[0].address, 128) == NISYROS_SUCCESS);
                for (int byte = 0; byte < 64; ++byte) assert(((unsigned char *)local[0].host)[byte] == 0x31);
                for (int byte = 64; byte < 128; ++byte) assert(((unsigned char *)local[0].host)[byte] == 0x42);
            }
        }
        nisyros_comm_cmd collective_replay = NULL;
        assert(nisyros_comm_cmd_create(endpoint, &collective_replay) == NISYROS_SUCCESS);
        assert(nisyros_comm_cmd_begin(collective_replay) == NISYROS_SUCCESS);
        for (uint32_t i = 0; i < local_count; ++i) {
            assert(nisyros_comm_cmd_all_reduce_slot(collective_replay, local[i].rank, 1,
                                                     local[i].address, local[i].address + 64,
                                                     4, NISYROS_DTYPE_U32,
                                                     NISYROS_REDUCE_SUM) == NISYROS_SUCCESS);
        }
        assert(nisyros_comm_cmd_end(collective_replay) == NISYROS_SUCCESS);
        for (uint64_t iteration = 1; iteration <= 5; ++iteration) {
            for (uint32_t i = 0; i < local_count; ++i) {
                for (int word = 0; word < 4; ++word) {
                    ((uint32_t *)local[i].host)[word] = local[i].rank + 1;
                    ((uint32_t *)((unsigned char *)local[i].host + 64))[word] = 0;
                }
                assert(nisyros_flush(local[i].device, local[i].address, 80) == NISYROS_SUCCESS);
            }
            nisyros_sequence_binding binding = {1, 0, iteration + 100};
            nisyros_comm_submit_desc submit_desc = {
                .header = {sizeof(submit_desc), NISYROS_ABI_VERSION, NULL},
                .binding_count = 1,
                .bindings = &binding,
            };
            nisyros_point reduced = {0};
            assert(nisyros_endpoint_submit(endpoint, collective_replay, &submit_desc,
                                           &reduced) == NISYROS_SUCCESS);
            assert(nisyros_wait(reduced, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            for (uint32_t i = 0; i < local_count; ++i) {
                assert(nisyros_invalidate(local[i].device, local[i].address + 64, 16) == NISYROS_SUCCESS);
                for (int word = 0; word < 4; ++word) {
                    assert(((uint32_t *)((unsigned char *)local[i].host + 64))[word] == 6);
                }
            }
        }
        assert(nisyros_comm_cmd_destroy(collective_replay) == NISYROS_SUCCESS);
        if (node == 1) printf("node 1 replayed five bound communication and reduction sequences\n");
        goto cleanup;
    }
    if (node == 1) {
        struct timespec delay = {0, 300000000};
        nanosleep(&delay, NULL);
    }
    nisyros_point done = {0};
    assert(nisyros_endpoint_submit(endpoint, cmd, NULL, &done) == NISYROS_SUCCESS);
    if (node == 0 && !peer_loss && !mismatch) {
        assert(nisyros_cmd_create(local[0].device, local[0].family, &dependent) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(dependent) == NISYROS_SUCCESS);
        assert(nisyros_cmd_wait(dependent, done) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(dependent, local[0].address, 64, 0x31313131) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(dependent) == NISYROS_SUCCESS);
        nisyros_cmd_info dependent_info = {.header = {sizeof(dependent_info), NISYROS_ABI_VERSION, NULL}};
        assert(nisyros_cmd_query(dependent, &dependent_info) == NISYROS_SUCCESS);
        assert(dependent_info.requires_host_progress == 1);
        nisyros_point rejected = {0};
        assert(nisyros_submit(local[0].queue, dependent, NULL, &rejected) == NISYROS_UNSUPPORTED);
        assert(rejected.event == NULL);
    }
    if (gpu_producer != NULL) {
        nisyros_point producer_done = {0};
        assert(nisyros_submit(local[0].queue, gpu_producer, NULL, &producer_done) == NISYROS_SUCCESS);
    }
    for (uint32_t i = 0; i < 1000000; ++i) host_work += i;
    nisyros_status wait_status = nisyros_wait(done, NISYROS_TIMEOUT_INFINITE);
    if (peer_loss || mismatch) {
        assert(wait_status == NISYROS_PEER_LOST);
        uint64_t value = 0;
        assert(nisyros_event_query(done.event, &value) == NISYROS_PEER_LOST);
        printf("%s terminated the pending wait without success\n",
               mismatch ? "byte-count mismatch" : "peer loss");
        goto cleanup;
    }
    assert(wait_status == NISYROS_SUCCESS);
    if (dependent != NULL) {
        nisyros_point dependent_done = {0};
        assert(nisyros_submit(local[0].queue, dependent, NULL, &dependent_done) == NISYROS_SUCCESS);
        assert(nisyros_wait(dependent_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    }
    if (node == 0) {
        assert(nisyros_invalidate(local[1].device, local[1].address + 64, 64) == NISYROS_SUCCESS);
        for (int byte = 64; byte < 128; ++byte) assert(((unsigned char *)local[1].host)[byte] == 0x31);
    }
    if (node == 1) {
        assert(nisyros_invalidate(local[0].device, local[0].address, 128) == NISYROS_SUCCESS);
        for (int byte = 0; byte < 64; ++byte) assert(((unsigned char *)local[0].host)[byte] == 0x31);
        for (int byte = 64; byte < 128; ++byte) assert(((unsigned char *)local[0].host)[byte] == 0x42);
        nisyros_alloc_desc consumed_desc = {
            .header = {sizeof(consumed_desc), NISYROS_ABI_VERSION, NULL},
            .bytes = 128,
            .alignment = 64,
            .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
            .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
        };
        nisyros_addr consumed = 0;
        void *consumed_host = NULL;
        assert(nisyros_alloc(local[0].device, &consumed_desc, &consumed) == NISYROS_SUCCESS);
        assert(nisyros_map(local[0].device, consumed, 128, &consumed_host) == NISYROS_SUCCESS);
        memset(consumed_host, 0, 128);
        assert(nisyros_flush(local[0].device, consumed, 128) == NISYROS_SUCCESS);
        nisyros_cmd consumer = NULL;
        assert(nisyros_cmd_create(local[0].device, local[0].family, &consumer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy(consumer, consumed, local[0].address, 128) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
        nisyros_point consumed_done = {0};
        assert(nisyros_submit(local[0].queue, consumer, NULL, &consumed_done) == NISYROS_SUCCESS);
        assert(nisyros_wait(consumed_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        assert(nisyros_invalidate(local[0].device, consumed, 128) == NISYROS_SUCCESS);
        for (int byte = 0; byte < 64; ++byte) assert(((unsigned char *)consumed_host)[byte] == 0x31);
        for (int byte = 64; byte < 128; ++byte) assert(((unsigned char *)consumed_host)[byte] == 0x42);
        assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
        assert(nisyros_unmap(local[0].device, consumed_host) == NISYROS_SUCCESS);
        assert(nisyros_free(local[0].device, consumed) == NISYROS_SUCCESS);
        printf("node 1 received two matched messages while host work continued\n");
    }
    for (uint32_t i = 0; i < local_count; ++i) {
        uint32_t value = local[i].rank == 0 ? UINT32_MAX : local[i].rank + 1;
        for (int word = 0; word < 4; ++word) ((uint32_t *)local[i].host)[word] = value;
        memset((unsigned char *)local[i].host + 64, 0, 64);
        assert(nisyros_flush(local[i].device, local[i].address, 128) == NISYROS_SUCCESS);
    }
    if (collective_timeout && node == 1) {
        struct timespec delay = {2, 0};
        nanosleep(&delay, NULL);
        goto cleanup;
    }
    nisyros_comm_cmd collective = NULL;
    assert(nisyros_comm_cmd_create(endpoint, &collective) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_begin(collective) == NISYROS_SUCCESS);
    const uint32_t types[4] = {NISYROS_DTYPE_U32, NISYROS_DTYPE_U32,
                               NISYROS_DTYPE_U32, NISYROS_DTYPE_I32};
    const uint32_t reductions[4] = {NISYROS_REDUCE_SUM, NISYROS_REDUCE_MIN,
                                    NISYROS_REDUCE_MAX, NISYROS_REDUCE_MIN};
    for (uint32_t op = 0; op < 4; ++op) {
        for (uint32_t i = 0; i < local_count; ++i) {
            uint64_t count = collective_mismatch && node == 1 && op == 0 ? 3 : 4;
            assert(nisyros_comm_cmd_all_reduce(collective, local[i].rank, op + 5,
                                                local[i].address, local[i].address + 64 + 16 * op,
                                                count, types[op], reductions[op]) == NISYROS_SUCCESS);
        }
    }
    assert(nisyros_comm_cmd_end(collective) == NISYROS_SUCCESS);
    nisyros_point collective_done = {0};
    assert(nisyros_endpoint_submit(endpoint, collective, NULL, &collective_done) == NISYROS_SUCCESS);
    nisyros_status collective_status = nisyros_wait(collective_done, NISYROS_TIMEOUT_INFINITE);
    if (collective_mismatch || collective_timeout) {
        assert(collective_status == NISYROS_PEER_LOST);
        uint64_t value = 0;
        assert(nisyros_event_query(collective_done.event, &value) == NISYROS_PEER_LOST);
        assert(nisyros_comm_cmd_destroy(collective) == NISYROS_SUCCESS);
        printf("%s terminated the collective wait without success\n",
               collective_mismatch ? "collective mismatch" : "collective timeout");
        goto cleanup;
    }
    assert(collective_status == NISYROS_SUCCESS);
    const uint32_t expected[4] = {4, 2, UINT32_MAX, UINT32_MAX};
    for (uint32_t i = 0; i < local_count; ++i) {
        assert(nisyros_invalidate(local[i].device, local[i].address + 64, 64) == NISYROS_SUCCESS);
        for (uint32_t op = 0; op < 4; ++op) {
            for (int word = 0; word < 4; ++word) {
                assert(((uint32_t *)((unsigned char *)local[i].host + 64 + 16 * op))[word] == expected[op]);
            }
        }
    }
    assert(nisyros_comm_cmd_destroy(collective) == NISYROS_SUCCESS);
    if (node == 1) printf("three ranks completed integer reductions\n");
cleanup:
    if (dependent != NULL) assert(nisyros_cmd_destroy(dependent) == NISYROS_SUCCESS);
    assert(nisyros_comm_cmd_destroy(cmd) == NISYROS_SUCCESS);
    if (gpu_producer != NULL) {
        assert(nisyros_cmd_destroy(gpu_producer) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(input_ready) == NISYROS_SUCCESS);
    }
    assert(nisyros_endpoint_close(endpoint) == NISYROS_SUCCESS);
    for (uint32_t i = 0; i < local_count; ++i) {
        assert(nisyros_unmap(local[i].device, local[i].host) == NISYROS_SUCCESS);
        assert(nisyros_free(local[i].device, local[i].address) == NISYROS_SUCCESS);
        assert(nisyros_device_close(local[i].device) == NISYROS_SUCCESS);
    }
    assert(nisyros_group_destroy(group) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    (void)host_work;
    return 0;
}
