#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static nisyros_device open_device(nisyros_cluster cluster, nisyros_device_id id,
                                  nisyros_queue *queue) {
    nisyros_queue_request request = {0, 1, 1.0f, 0};
    nisyros_device_open_desc open = NISYROS_DESC_INIT(nisyros_device_open_desc);
    open.queue_request_count = 1;
    open.queue_requests = &request;
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &open, &device) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(device, 0, 0, queue) == NISYROS_SUCCESS);
    return device;
}

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 1;
    nisyros_device_id id;
    nisyros_status inventory = nisyros_cluster_devices(cluster, &count, &id);
    if (inventory == NISYROS_INCOMPLETE) {
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        puts("ROCm peer mapping probe requires one GPU");
        return 77;
    }
    assert(inventory == NISYROS_SUCCESS && count == 1 && id.backend == NISYROS_BACKEND_ROCM);
    nisyros_queue source_queue = NULL, target_queue = NULL;
    nisyros_device source = open_device(cluster, id, &source_queue);
    nisyros_device target = open_device(cluster, id, &target_queue);
    nisyros_peer_info peer = NISYROS_DESC_INIT(nisyros_peer_info);
    assert(nisyros_peer_capabilities(target, source, &peer) == NISYROS_SUCCESS);
    assert(peer.same_physical_device);
    if (!peer.direct_mapping) {
        assert(nisyros_device_close(target) == NISYROS_SUCCESS);
        assert(nisyros_device_close(source) == NISYROS_SUCCESS);
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        puts("ROCm DMA-BUF peer mapping unavailable");
        return 77;
    }
    assert(peer.event_import);

    nisyros_alloc_desc alloc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    alloc.bytes = 64;
    alloc.alignment = 64;
    alloc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    nisyros_addr base = 0;
    assert(nisyros_alloc(source, &alloc, &base) == NISYROS_SUCCESS);
    uint32_t *host = NULL;
    assert(nisyros_map(source, base, 64, (void **)&host) == NISYROS_SUCCESS);
    memset(host, 0, 64);
    assert(nisyros_flush(source, base, 64) == NISYROS_SUCCESS);
    nisyros_location origin = {source, base};
    nisyros_addr imported = 0;
    assert(nisyros_peer_map(target, origin, 64,
                            NISYROS_ACCESS_READ | NISYROS_ACCESS_WRITE,
                            &imported) == NISYROS_SUCCESS);
    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(target, 0, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(command, imported, 64, 0x12345678) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit(target_queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(source, base, 64) == NISYROS_SUCCESS);
    for (int i = 0; i < 16; ++i) assert(host[i] == 0x12345678);

    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(target, imported) == NISYROS_SUCCESS);

    nisyros_event_desc event_desc = NISYROS_DESC_INIT(nisyros_event_desc);
    event_desc.signaling_domain = NISYROS_SIGNAL_DEVICE;
    nisyros_event produced = NULL, imported_event = NULL;
    assert(nisyros_event_create(source, &event_desc, &produced) == NISYROS_SUCCESS);
    nisyros_event_token token = NULL;
    assert(nisyros_event_export(produced, &token) == NISYROS_SUCCESS);
    assert(nisyros_event_import(target, token, &imported_event) == NISYROS_SUCCESS);
    origin.address = base;
    assert(nisyros_peer_map(target, origin, 64, NISYROS_ACCESS_WRITE,
                            &imported) == NISYROS_SUCCESS);
    nisyros_point signal = {produced, 1};
    nisyros_point wait = {imported_event, 1};
    nisyros_cmd producer = NULL, consumer = NULL;
    assert(nisyros_cmd_create(source, 0, &producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(producer, base, 64, 0x11111111) == NISYROS_SUCCESS);
    assert(nisyros_cmd_signal(producer, signal) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_create(target, 0, &consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_wait(consumer, wait) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(consumer, imported, 64, 0x22222222) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
    nisyros_point consumed = {0};
    assert(nisyros_submit(target_queue, consumer, NULL, &consumed) == NISYROS_SUCCESS);
    assert(nisyros_wait(consumed, 0) == NISYROS_TIMEOUT);
    assert(nisyros_submit(source_queue, producer, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(consumed, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(source, base, 64) == NISYROS_SUCCESS);
    for (int i = 0; i < 16; ++i) assert(host[i] == 0x22222222);
    assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
    assert(nisyros_event_token_destroy(token) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(imported_event) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(produced) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(target, imported) == NISYROS_SUCCESS);
    origin.address = base + 16;
    assert(nisyros_peer_map(target, origin, 16, NISYROS_ACCESS_WRITE,
                            &imported) == NISYROS_SUCCESS);
    assert(nisyros_cmd_create(target, 0, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(command, imported, 16, 0xaabbccdd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    assert(nisyros_submit(target_queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(source, base, 64) == NISYROS_SUCCESS);
    for (int i = 0; i < 16; ++i)
        assert(host[i] == (i >= 4 && i < 8 ? 0xaabbccdd : 0x22222222));
    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(target, imported) == NISYROS_SUCCESS);
    assert(nisyros_unmap(source, host) == NISYROS_SUCCESS);
    assert(nisyros_free(source, base) == NISYROS_SUCCESS);
    assert(nisyros_device_close(target) == NISYROS_SUCCESS);
    assert(nisyros_device_close(source) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("ROCm DMA-BUF peer mapping and timeline import verified");
    return 0;
}
