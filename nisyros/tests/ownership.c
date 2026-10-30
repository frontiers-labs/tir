#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 1;
    nisyros_device_id id;
    assert(nisyros_cluster_devices(cluster, &count, &id) == NISYROS_SUCCESS);
    assert(count == 1);
    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    uint32_t families[2] = {UINT32_MAX, UINT32_MAX};
    for (uint32_t family = 0; family < info.queue_family_count; ++family) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, family, &queue_info) == NISYROS_SUCCESS);
        if ((queue_info.flags & NISYROS_QUEUE_TRANSFER) && queue_info.count && families[1] == UINT32_MAX) {
            families[families[0] == UINT32_MAX ? 0 : 1] = family;
        }
    }
    if (!info.strict_profile_supported || families[1] == UINT32_MAX) {
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        puts("two transfer-capable queue families unavailable");
        return 77;
    }
    nisyros_queue_request requests[2] = {
        {families[0], 1, 1.0f, 0}, {families[1], 1, 1.0f, 0},
    };
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 2,
        .queue_requests = requests,
    };
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &open, &device) == NISYROS_SUCCESS);
    nisyros_queue queues[2] = {0};
    for (int i = 0; i < 2; ++i) {
        assert(nisyros_queue_get(device, families[i], 0, &queues[i]) == NISYROS_SUCCESS);
    }
    nisyros_alloc_sharing_extension sharing = {
        .base = {sizeof(sharing), NISYROS_EXTENSION_ALLOC_SHARING, 0, 0, NULL},
        .mode = NISYROS_SHARING_EXCLUSIVE,
    };
    nisyros_alloc_heap_extension heap = {
        .base = {sizeof(heap), NISYROS_EXTENSION_ALLOC_HEAP, 0, 0, &sharing.base},
        .required_heap = UINT32_MAX,
        .preferred_heap = UINT32_MAX,
    };
    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, &heap.base},
        .bytes = 64,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    nisyros_addr source = 0, destination = 0;
    assert(nisyros_alloc(device, &allocation, &source) == NISYROS_SUCCESS);
    nisyros_allocation_info allocation_info = {.header = {sizeof(allocation_info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_allocation_query(device, source, &allocation_info) == NISYROS_SUCCESS);
    assert(allocation_info.exclusive == 1);
    allocation.header.extensions = NULL;
    assert(nisyros_alloc(device, &allocation, &destination) == NISYROS_SUCCESS);
    assert(nisyros_allocation_query(device, destination, &allocation_info) == NISYROS_SUCCESS);
    assert(allocation_info.exclusive == 0);
    void *destination_host = NULL;
    assert(nisyros_map(device, destination, 64, &destination_host) == NISYROS_SUCCESS);
    memset(destination_host, 0, 64);
    assert(nisyros_flush(device, destination, 64) == NISYROS_SUCCESS);
    nisyros_barrier_desc release = {
        .header = {sizeof(release), NISYROS_ABI_VERSION, NULL},
        .address = source, .bytes = 64,
        .src_stages = NISYROS_BARRIER_STAGE_TRANSFER,
        .src_access = NISYROS_ACCESS_WRITE,
        .src_family = families[0], .dst_family = families[1],
    };
    nisyros_barrier_desc acquire = release;
    acquire.src_stages = 0;
    acquire.src_access = 0;
    acquire.dst_stages = NISYROS_BARRIER_STAGE_TRANSFER;
    acquire.dst_access = NISYROS_ACCESS_READ;
    nisyros_event_desc event_desc = {
        .header = {sizeof(event_desc), NISYROS_ABI_VERSION, NULL},
        .signaling_domain = NISYROS_SIGNAL_DEVICE,
    };
    nisyros_event released = NULL;
    assert(nisyros_event_create(device, &event_desc, &released) == NISYROS_SUCCESS);
    nisyros_cmd producer = NULL, consumer = NULL;
    assert(nisyros_cmd_create(device, families[0], &producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(producer, source, 64, 0x64646464) == NISYROS_SUCCESS);
    assert(nisyros_cmd_barrier_scoped(producer, &release) == NISYROS_SUCCESS);
    assert(nisyros_cmd_signal(producer, (nisyros_point){released, 1}) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_create(device, families[1], &consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_wait(consumer, (nisyros_point){released, 1}) == NISYROS_SUCCESS);
    assert(nisyros_cmd_barrier_scoped(consumer, &acquire) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy(consumer, destination, source, 64) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
    nisyros_point consumed = {0}, produced = {0};
    assert(nisyros_submit(queues[1], consumer, NULL, &consumed) == NISYROS_SUCCESS);
    assert(nisyros_submit(queues[0], producer, NULL, &produced) == NISYROS_SUCCESS);
    assert(nisyros_wait(consumed, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_wait(produced, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, destination, 64) == NISYROS_SUCCESS);
    for (int i = 0; i < 64; ++i) assert(((unsigned char *)destination_host)[i] == 0x64);
    assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(released) == NISYROS_SUCCESS);
    nisyros_cmd invalid = NULL;
    assert(nisyros_cmd_create(device, families[0], &invalid) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(invalid) == NISYROS_SUCCESS);
    release.address = destination;
    assert(nisyros_cmd_barrier_scoped(invalid, &release) == NISYROS_UNSUPPORTED);
    assert(nisyros_cmd_end(invalid) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_destroy(invalid) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, destination_host) == NISYROS_SUCCESS);
    assert(nisyros_free(device, source) == NISYROS_SUCCESS);
    assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("exclusive allocation moved between queue families with release/acquire barriers");
    return 0;
}
