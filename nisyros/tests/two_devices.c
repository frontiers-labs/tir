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
        assert(nisyros_unmap(sessions[i].device, sessions[i].host) == NISYROS_SUCCESS);
        assert(nisyros_free(sessions[i].device, sessions[i].address) == NISYROS_SUCCESS);
        assert(nisyros_device_close(sessions[i].device) == NISYROS_SUCCESS);
    }
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    printf("two logical Vulkan devices completed independent submissions; same physical UUID=%d\n",
           memcmp(info[0].device_uuid, info[1].device_uuid, 16) == 0);
    return 0;
}
