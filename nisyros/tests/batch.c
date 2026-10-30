#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static nisyros_cmd command(nisyros_device device, uint32_t family) {
    nisyros_cmd cmd = NULL;
    assert(nisyros_cmd_create(device, family, &cmd) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
    return cmd;
}

int main(void) {
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 1;
    nisyros_device_id id;
    assert(nisyros_cluster_devices(cluster, &count, &id) == NISYROS_SUCCESS);
    assert(count == 1);
    nisyros_device_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cluster_device_info(cluster, id, &info) == NISYROS_SUCCESS);
    assert(info.strict_profile_supported == 1);
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
    nisyros_queue queue = NULL;
    assert(nisyros_queue_get(device, family, 0, &queue) == NISYROS_SUCCESS);
    nisyros_alloc_desc alloc = {
        .header = {sizeof(alloc), NISYROS_ABI_VERSION, NULL},
        .bytes = 64,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    nisyros_addr address = 0;
    assert(nisyros_alloc(device, &alloc, &address) == NISYROS_SUCCESS);
    uint32_t *mapped = NULL;
    assert(nisyros_map(device, address, 64, (void **)&mapped) == NISYROS_SUCCESS);
    nisyros_event_desc device_event_desc = {
        .header = {sizeof(device_event_desc), NISYROS_ABI_VERSION, NULL},
        .signaling_domain = NISYROS_SIGNAL_DEVICE,
    };
    nisyros_event_desc host_event_desc = {
        .header = {sizeof(host_event_desc), NISYROS_ABI_VERSION, NULL},
        .signaling_domain = NISYROS_SIGNAL_HOST,
    };
    nisyros_event stage = NULL, finished = NULL, host = NULL;
    assert(nisyros_event_create(device, &device_event_desc, &stage) == NISYROS_SUCCESS);
    assert(nisyros_event_create(device, &device_event_desc, &finished) == NISYROS_SUCCESS);
    assert(nisyros_event_create(device, &host_event_desc, &host) == NISYROS_SUCCESS);
    nisyros_cmd producer = command(device, family);
    assert(nisyros_cmd_fill(producer, address, 16, 0x11111111) == NISYROS_SUCCESS);
    assert(nisyros_cmd_signal_slot(producer, 0) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
    nisyros_cmd consumer = command(device, family);
    assert(nisyros_cmd_wait_slot(consumer, 0) == NISYROS_SUCCESS);
    assert(nisyros_cmd_copy(consumer, address + 16, address, 16) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
    nisyros_cmd_info producer_info = {.header = {sizeof(producer_info), NISYROS_ABI_VERSION, NULL}};
    nisyros_cmd_info consumer_info = {.header = {sizeof(consumer_info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cmd_query(producer, &producer_info) == NISYROS_SUCCESS);
    assert(nisyros_cmd_query(consumer, &consumer_info) == NISYROS_SUCCESS);
    assert(producer_info.native_record_count == 1 && consumer_info.native_record_count == 1);
    for (uint64_t iteration = 1; iteration <= 6; ++iteration) {
        memset(mapped, 0, 64);
        assert(nisyros_flush(device, address, 64) == NISYROS_SUCCESS);
        nisyros_event_binding producer_binding = {0, 0, {stage, iteration}};
        nisyros_event_binding consumer_binding = {0, 0, {stage, iteration}};
        nisyros_submit_desc producer_submit = {
            .header = {sizeof(producer_submit), NISYROS_ABI_VERSION, NULL},
            .binding_count = 1,
            .bindings = &producer_binding,
        };
        nisyros_submit_desc consumer_submit = {
            .header = {sizeof(consumer_submit), NISYROS_ABI_VERSION, NULL},
            .binding_count = 1,
            .bindings = &consumer_binding,
        };
        nisyros_batch_entry entries[2] = {{producer, &producer_submit}, {consumer, &consumer_submit}};
        nisyros_point prerequisites[2] = {{host, 0}, {host, 1}};
        nisyros_point completed = {finished, iteration};
        nisyros_batch_submit_desc batch = NISYROS_BATCH_SUBMIT_DESC_INIT;
        batch.command_count = 2;
        batch.commands = entries;
        batch.wait_count = 2;
        batch.waits = prerequisites;
        batch.signal_count = 1;
        batch.signals = &completed;
        nisyros_point done = {0};
        if (iteration == 6) {
            entries[1].submit = NULL;
            assert(nisyros_submit_batch(queue, &batch, &done) == NISYROS_INVALID_ARGUMENT);
            assert(done.event == NULL);
            uint64_t current = 0;
            assert(nisyros_event_query(stage, &current) == NISYROS_SUCCESS && current == 5);
            assert(nisyros_event_query(finished, &current) == NISYROS_SUCCESS && current == 5);
            assert(nisyros_invalidate(device, address, 64) == NISYROS_SUCCESS);
            for (int word = 0; word < 16; ++word) assert(mapped[word] == 0);
            entries[1].submit = &consumer_submit;
        }
        nisyros_status submit_status = nisyros_submit_batch(queue, &batch, &done);
        if (submit_status != NISYROS_SUCCESS) fprintf(stderr, "batch iteration %llu: %d\n",
                                                      (unsigned long long)iteration, submit_status);
        assert(submit_status == NISYROS_SUCCESS);
        if (iteration == 1) {
            assert(nisyros_cmd_reset(producer) == NISYROS_BUSY);
            assert(nisyros_event_destroy(host) == NISYROS_BUSY);
            assert(nisyros_event_destroy(finished) == NISYROS_BUSY);
            assert(nisyros_event_host_signal(host, 1) == NISYROS_SUCCESS);
        }
        assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        assert(nisyros_wait(completed, 0) == NISYROS_SUCCESS);
        assert(nisyros_invalidate(device, address, 64) == NISYROS_SUCCESS);
        for (int word = 0; word < 8; ++word) assert(mapped[word] == 0x11111111);
    }
    nisyros_cmd_info after = {.header = {sizeof(after), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cmd_query(producer, &after) == NISYROS_SUCCESS);
    assert(after.native_record_count == producer_info.native_record_count);
    assert(nisyros_cmd_query(consumer, &after) == NISYROS_SUCCESS);
    assert(after.native_record_count == consumer_info.native_record_count);

    nisyros_batch_entry duplicated[2] = {{producer, NULL}, {producer, NULL}};
    nisyros_batch_submit_desc invalid = NISYROS_BATCH_SUBMIT_DESC_INIT;
    invalid.command_count = 2;
    invalid.commands = duplicated;
    nisyros_point rejected = {0};
    assert(nisyros_submit_batch(queue, &invalid, &rejected) == NISYROS_INVALID_ARGUMENT);
    assert(rejected.event == NULL);
    nisyros_event_binding producer_binding = {0, 0, {stage, 7}};
    nisyros_event_binding consumer_binding = {0, 0, {stage, 7}};
    nisyros_submit_desc producer_submit = {
        .header = {sizeof(producer_submit), NISYROS_ABI_VERSION, NULL},
        .binding_count = 1, .bindings = &producer_binding,
    };
    nisyros_submit_desc consumer_submit = {
        .header = {sizeof(consumer_submit), NISYROS_ABI_VERSION, NULL},
        .binding_count = 1, .bindings = &consumer_binding,
    };
    nisyros_batch_entry entries[2] = {{producer, &producer_submit}, {consumer, &consumer_submit}};
    invalid.commands = entries;
    nisyros_point bad_signal = {host, 2};
    invalid.signal_count = 1;
    invalid.signals = &bad_signal;
    assert(nisyros_submit_batch(queue, &invalid, &rejected) == NISYROS_INVALID_ARGUMENT);
    assert(rejected.event == NULL);

    nisyros_batch_entry reversed[2] = {{consumer, &consumer_submit}, {producer, &producer_submit}};
    invalid.commands = reversed;
    invalid.signal_count = 0;
    assert(nisyros_submit_batch(queue, &invalid, &rejected) == NISYROS_INVALID_ARGUMENT);
    assert(rejected.event == NULL);
    invalid.commands = entries;

    nisyros_point repeated_signals[2] = {{finished, 7}, {finished, 8}};
    invalid.signal_count = 2;
    invalid.signals = repeated_signals;
    memset(mapped, 0, 64);
    assert(nisyros_flush(device, address, 64) == NISYROS_SUCCESS);
    nisyros_point done = {0};
    assert(nisyros_submit_batch(queue, &invalid, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_wait(repeated_signals[1], 0) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, address, 64) == NISYROS_SUCCESS);
    for (int word = 0; word < 8; ++word) assert(mapped[word] == 0x11111111);

    producer_binding.point.value = 8;
    consumer_binding.point.value = 8;
    repeated_signals[0].value = 10;
    repeated_signals[1].value = 9;
    memset(mapped, 0, 64);
    assert(nisyros_flush(device, address, 64) == NISYROS_SUCCESS);
    assert(nisyros_submit_batch(queue, &invalid, &rejected) == NISYROS_INVALID_ARGUMENT);
    assert(rejected.event == NULL);
    uint64_t current = 0;
    assert(nisyros_event_query(stage, &current) == NISYROS_SUCCESS && current == 7);
    assert(nisyros_event_query(finished, &current) == NISYROS_SUCCESS && current == 8);
    assert(nisyros_invalidate(device, address, 64) == NISYROS_SUCCESS);
    for (int word = 0; word < 16; ++word) assert(mapped[word] == 0);
    repeated_signals[0].value = 9;
    repeated_signals[1].value = 10;
    assert(nisyros_submit_batch(queue, &invalid, &done) == NISYROS_SUCCESS);
    assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
    assert(nisyros_wait(repeated_signals[1], 0) == NISYROS_SUCCESS);
    assert(nisyros_invalidate(device, address, 64) == NISYROS_SUCCESS);
    for (int word = 0; word < 8; ++word) assert(mapped[word] == 0x11111111);

    assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
    assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(stage) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(finished) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(host) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, mapped) == NISYROS_SUCCESS);
    assert(nisyros_free(device, address) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    puts("batch validation, dependencies, replay, and shared signals passed");
    return 0;
}
