#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static void check_diagnostic(nisyros_status status, int32_t backend_code) {
    nisyros_diagnostic_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    uint32_t required = 0;
    assert(nisyros_diagnostic_get(&info, &required, NULL) == NISYROS_SUCCESS);
    assert(info.status == status && info.backend_code == backend_code && required > 4);
    char short_message[4];
    uint32_t capacity = sizeof(short_message);
    assert(nisyros_diagnostic_get(&info, &capacity, short_message) == NISYROS_INCOMPLETE);
    assert(capacity == required && short_message[3] == 0);
    char message[128];
    capacity = sizeof(message);
    assert(nisyros_diagnostic_get(&info, &capacity, message) == NISYROS_SUCCESS);
    assert(capacity == required && message[0] != 0);
}

int main(int argc, char **argv) {
    int wait_loss = argc == 2 && strcmp(argv[1], "wait") == 0;
    int oom = argc == 2 && strcmp(argv[1], "oom") == 0;
    assert(argc == 1 || wait_loss || oom);
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 1;
    nisyros_device_id id;
    assert(nisyros_cluster_devices(cluster, &count, &id) == NISYROS_SUCCESS);
    assert(count == 1);
    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    uint32_t family = UINT32_MAX;
    for (uint32_t i = 0; i < info.queue_family_count; ++i) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, id, i, &queue_info) == NISYROS_SUCCESS);
        if (queue_info.flags & NISYROS_QUEUE_COMPUTE) { family = i; break; }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, id, &open, &device) == NISYROS_SUCCESS);
    assert(nisyros_device_abort(device) == NISYROS_INVALID_ARGUMENT);
    nisyros_queue queue = NULL;
    assert(nisyros_queue_get(device, family, 0, &queue) == NISYROS_SUCCESS);
    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = 64,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
    };
    nisyros_addr address = 0;
    assert(nisyros_alloc(device, &allocation, &address) == NISYROS_SUCCESS);
    nisyros_event_desc event_desc = {
        .header = {sizeof(event_desc), NISYROS_ABI_VERSION, NULL},
        .signaling_domain = NISYROS_SIGNAL_DEVICE,
    };
    nisyros_event event = NULL;
    assert(nisyros_event_create(device, &event_desc, &event) == NISYROS_SUCCESS);
    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(device, family, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_fill(command, address, 64, 0x12345678) == NISYROS_SUCCESS);
    assert(nisyros_cmd_signal(command, (nisyros_point){event, 1}) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(command) == NISYROS_SUCCESS);
    nisyros_point returned = {0};
    nisyros_status submit_status = nisyros_submit(queue, command, NULL, &returned);
    if (oom) {
        assert(submit_status == NISYROS_OUT_OF_MEMORY && returned.event == NULL);
        check_diagnostic(NISYROS_OUT_OF_MEMORY, -1);
        uint64_t value = UINT64_MAX;
        assert(nisyros_event_query(event, &value) == NISYROS_SUCCESS && value == 0);
        assert(nisyros_device_abort(device) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_submit(queue, command, NULL, &returned) == NISYROS_SUCCESS);
        assert(returned.event != NULL && returned.value == 1);
        assert(nisyros_wait(returned, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        assert(nisyros_wait((nisyros_point){event, 1}, 0) == NISYROS_SUCCESS);
        assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(event) == NISYROS_SUCCESS);
        assert(nisyros_free(device, address) == NISYROS_SUCCESS);
        assert(nisyros_device_close(device) == NISYROS_SUCCESS);
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        puts("out-of-memory submit rolled back and retried without poisoned state");
        return 0;
    }
    if (wait_loss) {
        assert(submit_status == NISYROS_SUCCESS && returned.event != NULL);
        assert(nisyros_wait(returned, NISYROS_TIMEOUT_INFINITE) == NISYROS_DEVICE_LOST);
        check_diagnostic(NISYROS_DEVICE_LOST, -4);
        assert(nisyros_wait((nisyros_point){event, 1}, 0) == NISYROS_DEVICE_LOST);
    } else {
        assert(submit_status == NISYROS_SUBMISSION_FAILED);
        check_diagnostic(NISYROS_SUBMISSION_FAILED, -4);
        assert(returned.event == NULL);
        assert(nisyros_wait((nisyros_point){event, 1}, 0) == NISYROS_SUBMISSION_FAILED);
    }
    uint64_t value = 0;
    assert(nisyros_event_query(event, &value) ==
           (wait_loss ? NISYROS_DEVICE_LOST : NISYROS_SUBMISSION_FAILED));
    assert(nisyros_cmd_destroy(command) ==
           (wait_loss ? NISYROS_DEVICE_LOST : NISYROS_SUBMISSION_FAILED));
    assert(nisyros_event_destroy(event) == NISYROS_BUSY);
    assert(nisyros_free(device, address) == NISYROS_BUSY);
    assert(nisyros_device_close(device) == NISYROS_BUSY);
    assert(nisyros_device_abort(device) == NISYROS_SUCCESS);
    assert(nisyros_device_abort(device) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(command) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(event) == NISYROS_SUCCESS);
    assert(nisyros_free(device, address) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("lost device aborted and retained resources released after idle");
    return 0;
}
