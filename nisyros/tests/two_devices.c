#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef struct {
    nisyros_device device;
    nisyros_queue queue;
    uint32_t family;
    nisyros_addr address;
    void *host;
    nisyros_cmd cmd;
    nisyros_point done;
} session;

static void open_session(nisyros_cluster cluster, nisyros_device_id id,
                         nisyros_device_info *info, session *session) {
    *info = (nisyros_device_info){.header = {sizeof(*info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, info) == NISYROS_SUCCESS);
    assert(info->strict_profile_supported == 1);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info->queue_family_count; ++i) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, i, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_COMPUTE) { family = i; break; }
    }
    assert(family != UINT32_MAX);
    session->family = family;
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    assert(nisyros_device_open(cluster, id, &open, &session->device) == NISYROS_SUCCESS);
    assert(nisyros_queue_get(session->device, family, 0, &session->queue) == NISYROS_SUCCESS);
    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = 64,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    assert(nisyros_alloc(session->device, &allocation, &session->address) == NISYROS_SUCCESS);
    assert(nisyros_map(session->device, session->address, 64, &session->host) == NISYROS_SUCCESS);
}

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count >= 2);
    nisyros_device_id ids[2];
    count = 2;
    nisyros_status status = nisyros_cluster_devices(cluster, &count, ids);
    assert(status == NISYROS_SUCCESS || status == NISYROS_INCOMPLETE);
    assert(ids[0].index != ids[1].index);
    nisyros_device_info info[2];
    session sessions[2] = {0};
    for (int i = 0; i < 2; ++i) {
        open_session(cluster, ids[i], &info[i], &sessions[i]);
        memset(sessions[i].host, 0, 64);
        assert(nisyros_flush(sessions[i].device, sessions[i].address, 64) == NISYROS_SUCCESS);
        assert(nisyros_cmd_create(sessions[i].device, sessions[i].family,
                                  &sessions[i].cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(sessions[i].cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(sessions[i].cmd, sessions[i].address, 64,
                                i == 0 ? 0x11111111 : 0x22222222) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(sessions[i].cmd) == NISYROS_SUCCESS);
        assert(nisyros_submit(sessions[i].queue, sessions[i].cmd, NULL,
                              &sessions[i].done) == NISYROS_SUCCESS);
    }
    assert(nisyros_wait_many(2, (nisyros_point[]){sessions[0].done, sessions[1].done},
                             NISYROS_WAIT_ALL, NISYROS_TIMEOUT_INFINITE, NULL) == NISYROS_SUCCESS);
    for (int i = 0; i < 2; ++i) {
        assert(nisyros_invalidate(sessions[i].device, sessions[i].address, 64) == NISYROS_SUCCESS);
        for (int byte = 0; byte < 64; ++byte) {
            assert(((unsigned char *)sessions[i].host)[byte] == (i == 0 ? 0x11 : 0x22));
        }
        assert(nisyros_cmd_destroy(sessions[i].cmd) == NISYROS_SUCCESS);
    }

    nisyros_peer_info peer = {.header = {sizeof(peer), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_peer_capabilities(sessions[0].device, sessions[1].device, &peer) == NISYROS_SUCCESS);
    assert(peer.same_physical_device == 1 && peer.direct_mapping == 1 && peer.event_import == 1);
    nisyros_addr imported = 0;
    nisyros_location source = {sessions[0].device, sessions[0].address};
    assert(nisyros_peer_map(sessions[1].device, source, 64, NISYROS_ACCESS_READ,
                            &imported) == NISYROS_SUCCESS);
    assert(imported != 0);
    assert(nisyros_free(sessions[0].device, sessions[0].address) == NISYROS_BUSY);
    nisyros_event_desc event_desc = {
        .header = {sizeof(event_desc), NISYROS_ABI_VERSION, NULL},
        .signaling_domain = NISYROS_SIGNAL_DEVICE,
    };
    nisyros_event produced = NULL, produced_in_peer = NULL;
    nisyros_event_token token = NULL;
    assert(nisyros_event_create(sessions[0].device, &event_desc, &produced) == NISYROS_SUCCESS);
    assert(nisyros_event_export(produced, &token) == NISYROS_SUCCESS);
    assert(nisyros_event_import(sessions[1].device, token, &produced_in_peer) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(produced) == NISYROS_BUSY);
    nisyros_cmd producer = NULL, consumer = NULL;
    assert(nisyros_cmd_create(sessions[0].device, sessions[0].family, &producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(producer, sessions[0].address, 64, 0x44444444) == NISYROS_SUCCESS);
    assert(nisyros_cmd_signal(producer, (nisyros_point){produced, 1}) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_create(sessions[1].device, sessions[1].family, &consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_wait(consumer, (nisyros_point){produced_in_peer, 1}) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy(consumer, sessions[1].address, imported, 64) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(sessions[1].device, imported) == NISYROS_BUSY);
    nisyros_point copied = {0}, producer_done = {0};
    assert(nisyros_submit(sessions[1].queue, consumer, NULL, &copied) == NISYROS_SUCCESS);
    assert(nisyros_submit(sessions[0].queue, producer, NULL, &producer_done) == NISYROS_SUCCESS);
    assert(nisyros_wait(copied, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_wait(producer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(sessions[1].device, sessions[1].address, 64) == NISYROS_SUCCESS);
    for (int byte = 0; byte < 64; ++byte) {
        assert(((unsigned char *)sessions[1].host)[byte] == 0x44);
    }
    assert(nisyros_event_token_destroy(token) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(produced) == NISYROS_BUSY);
    assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(produced_in_peer) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(produced) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(sessions[1].device, imported) == NISYROS_SUCCESS);

    nisyros_location interior_source = {sessions[0].device, sessions[0].address + 8};
    nisyros_addr interior = 0;
    assert(nisyros_peer_map(sessions[1].device, interior_source, 16,
                            NISYROS_ACCESS_READ, &interior) == NISYROS_SUCCESS);
    nisyros_cmd read_only_write = NULL;
    assert(nisyros_cmd_create(sessions[1].device, sessions[1].family,
                              &read_only_write) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(read_only_write) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(read_only_write, interior, 4, 0) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_end(read_only_write) == NISYROS_INVALID_ARGUMENT);
    assert(nisyros_cmd_destroy(read_only_write) == NISYROS_SUCCESS);
    memset((unsigned char *)sessions[1].host + 16, 0, 16);
    assert(nisyros_flush(sessions[1].device, sessions[1].address + 16, 16) == NISYROS_SUCCESS);
    nisyros_cmd interior_copy = NULL;
    assert(nisyros_cmd_create(sessions[1].device, sessions[1].family,
                              &interior_copy) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(interior_copy) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy(interior_copy, sessions[1].address + 16, interior, 16) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(interior_copy) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(sessions[1].device, interior) == NISYROS_BUSY);
    nisyros_point interior_done = {0};
    assert(nisyros_submit(sessions[1].queue, interior_copy, NULL, &interior_done) == NISYROS_SUCCESS);
    assert(nisyros_wait(interior_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(sessions[1].device, sessions[1].address + 16, 16) == NISYROS_SUCCESS);
    for (int byte = 16; byte < 32; ++byte) {
        assert(((unsigned char *)sessions[1].host)[byte] == 0x44);
    }
    assert(nisyros_cmd_destroy(interior_copy) == NISYROS_SUCCESS);
    assert(nisyros_peer_unmap(sessions[1].device, interior) == NISYROS_SUCCESS);

    nisyros_cluster other_cluster = NULL;
    assert(nisyros_cluster_open_local(&other_cluster) == NISYROS_SUCCESS);
    nisyros_device_id other_id;
    uint32_t other_count = 1;
    assert(nisyros_cluster_devices(other_cluster, &other_count, &other_id) == NISYROS_INCOMPLETE);
    session outsider = {0};
    nisyros_device_info outsider_info;
    open_session(other_cluster, other_id, &outsider_info, &outsider);
    peer = (nisyros_peer_info){.header = {sizeof(peer), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_peer_capabilities(sessions[0].device, outsider.device, &peer) == NISYROS_SUCCESS);
    assert(peer.same_physical_device == 1 && peer.direct_mapping == 0);
    nisyros_addr refused = 123;
    assert(nisyros_peer_map(outsider.device, source, 64, NISYROS_ACCESS_READ,
                            &refused) == NISYROS_UNSUPPORTED);
    assert(refused == 0);
    assert(nisyros_unmap(outsider.device, outsider.host) == NISYROS_SUCCESS);
    assert(nisyros_free(outsider.device, outsider.address) == NISYROS_SUCCESS);
    assert(nisyros_device_close(outsider.device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(other_cluster) == NISYROS_SUCCESS);

    for (int i = 0; i < 2; ++i) {
        assert(nisyros_unmap(sessions[i].device, sessions[i].host) == NISYROS_SUCCESS);
        assert(nisyros_free(sessions[i].device, sessions[i].address) == NISYROS_SUCCESS);
        assert(nisyros_device_close(sessions[i].device) == NISYROS_SUCCESS);
    }
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    printf("two logical Vulkan devices completed independent submissions; same physical UUID=%d\n",
           memcmp(info[0].device_uuid, info[1].device_uuid, 16) == 0);
    return 0;
}
