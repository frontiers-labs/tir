#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

static nisyros_device open_device(nisyros_cluster cluster, nisyros_device_id id,
                                  nisyros_queue *queue) {
    nisyros_queue_request request = {0, 1, 1.0f, 0};
    nisyros_device_open_desc desc = NISYROS_DESC_INIT(nisyros_device_open_desc);
    desc.queue_request_count = 1;
    desc.queue_requests = &request;
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &desc, &device) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(device, 0, 0, queue) == NISYROS_SUCCESS);
    return device;
}

static nisyros_addr allocate(nisyros_device device) {
    nisyros_alloc_desc desc = NISYROS_DESC_INIT(nisyros_alloc_desc);
    desc.bytes = 4096;
    desc.alignment = 4096;
    desc.required_properties = NISYROS_MEMORY_HOST_VISIBLE;
    nisyros_addr address = 0;
    assert(nisyros_alloc(device, &desc, &address) == NISYROS_SUCCESS);
    return address;
}

static void fill(nisyros_device device, nisyros_queue queue,
                 nisyros_addr address, uint64_t bytes, uint32_t pattern) {
    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(device, 0, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(command, address, bytes, pattern) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit(queue, command, NULL, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
}

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    nisyros_device_id ids[8];
    uint32_t count = 8;
    assert(nisyros_cluster_devices(cluster, &count, ids) == NISYROS_SUCCESS);
    nisyros_device_id vulkan_id = {0}, xdna_id = {0};
    for (uint32_t i = 0; i < count; ++i) {
        if (ids[i].backend == NISYROS_BACKEND_VULKAN) vulkan_id = ids[i];
        if (ids[i].backend == NISYROS_BACKEND_XDNA) xdna_id = ids[i];
    }
    assert(vulkan_id.backend && xdna_id.backend);
    nisyros_queue vulkan_queue = NULL, xdna_queue = NULL;
    nisyros_device vulkan = open_device(cluster, vulkan_id, &vulkan_queue);
    nisyros_device xdna = open_device(cluster, xdna_id, &xdna_queue);
    nisyros_peer_info peer = NISYROS_DESC_INIT(nisyros_peer_info);

    assert(nisyros_peer_capabilities(vulkan, xdna, &peer) == NISYROS_SUCCESS);
    assert(!peer.same_physical_device && peer.direct_mapping);
    assert(peer.shared_allocation_backend == NISYROS_BACKEND_XDNA);
    nisyros_addr xdna_base = allocate(xdna);
    nisyros_location source = {xdna, xdna_base};
    nisyros_addr vulkan_view = 0;
    assert(nisyros_peer_map(vulkan, source, 4096, NISYROS_ACCESS_WRITE,
                            &vulkan_view) == NISYROS_SUCCESS);
    fill(vulkan, vulkan_queue, vulkan_view, 4096, 0xaabbccdd);
    assert(nisyros_invalidate(xdna, xdna_base, 4096) == NISYROS_SUCCESS);
    uint32_t *host = NULL;
    assert(nisyros_map(xdna, xdna_base, 4096, (void **)&host) == NISYROS_SUCCESS);
    for (int i = 0; i < 1024; ++i) assert(host[i] == 0xaabbccdd);
    assert(nisyros_unmap(xdna, host) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(vulkan, vulkan_view) == NISYROS_SUCCESS);
    source.address = xdna_base + 16;
    assert(nisyros_peer_map(vulkan, source, 16, NISYROS_ACCESS_WRITE,
                            &vulkan_view) == NISYROS_SUCCESS);
    fill(vulkan, vulkan_queue, vulkan_view, 16, 0x11223344);
    assert(nisyros_invalidate(xdna, xdna_base, 4096) == NISYROS_SUCCESS);
    assert(nisyros_map(xdna, xdna_base, 4096, (void **)&host) == NISYROS_SUCCESS);
    assert(host[0] == 0xaabbccdd && host[3] == 0xaabbccdd);
    for (int i = 4; i < 8; ++i) assert(host[i] == 0x11223344);
    assert(host[8] == 0xaabbccdd);
    assert(nisyros_unmap(xdna, host) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(vulkan, vulkan_view) == NISYROS_SUCCESS);
    assert(nisyros_free(xdna, xdna_base) == NISYROS_SUCCESS);

    assert(nisyros_peer_capabilities(xdna, vulkan, &peer) == NISYROS_SUCCESS);
    assert(peer.direct_mapping && peer.shared_allocation_backend == NISYROS_BACKEND_XDNA);
    nisyros_addr vulkan_base = allocate(vulkan);
    source = (nisyros_location){vulkan, vulkan_base};
    nisyros_addr xdna_view = 0;
    assert(nisyros_peer_map(xdna, source, 4096, NISYROS_ACCESS_WRITE,
                            &xdna_view) == NISYROS_SUCCESS);
    fill(xdna, xdna_queue, xdna_view, 4096, 0x12345678);
    assert(nisyros_map(vulkan, vulkan_base, 4096, (void **)&host) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(vulkan, vulkan_base, 4096) == NISYROS_SUCCESS);
    for (int i = 0; i < 1024; ++i) assert(host[i] == 0x12345678);
    assert(nisyros_unmap(vulkan, host) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(xdna, xdna_view) == NISYROS_SUCCESS);
    assert(nisyros_free(vulkan, vulkan_base) == NISYROS_SUCCESS);

    assert(nisyros_device_close(xdna) == NISYROS_SUCCESS);
    assert(nisyros_device_close(vulkan) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("Vulkan and XDNA imported each other's DMA-BUFs and verified output");
    return 0;
}
