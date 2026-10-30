#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    assert(argc == 2);
    assert(strcmp(argv[1], "strict") == 0 || strcmp(argv[1], "unsupported") == 0 ||
           strcmp(argv[1], "auto") == 0);
    int expect_strict = strcmp(argv[1], "strict") == 0;
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    assert(cluster != NULL);

    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count > 0 && count <= 16);
    nisyros_device_id ids[16];
    uint32_t capacity = 0;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_INCOMPLETE);
    assert(capacity == count);
    capacity = 16;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_SUCCESS);

    nisyros_device_info info = {
        .header = {sizeof(info), NISYROS_ABI_VERSION, NULL},
    };
    assert(nisyros_cluster_device_info(cluster, ids[0], &info) == NISYROS_SUCCESS);
    nisyros_extension unknown = {sizeof(unknown), 0xfeed, NISYROS_EXTENSION_OPTIONAL, 0, NULL};
    info.header.extensions = &unknown;
    assert(nisyros_cluster_device_info(cluster, ids[0], &info) == NISYROS_SUCCESS);
    unknown.flags = 0;
    assert(nisyros_cluster_device_info(cluster, ids[0], &info) == NISYROS_UNSUPPORTED);
    info.header.extensions = NULL;
    assert(info.backend == NISYROS_BACKEND_VULKAN);
    if (strcmp(argv[1], "strict") == 0) {
        assert(info.device_type != NISYROS_DEVICE_TYPE_CPU);
    }
    if (strcmp(argv[1], "auto") == 0) expect_strict = info.strict_profile_supported != 0;
    assert((info.strict_profile_supported != 0) == expect_strict);
    printf("%s driver=%s strict=%u families=%u\n", info.name, info.driver_name,
           info.strict_profile_supported, info.queue_family_count);

    uint32_t selected = UINT32_MAX;
    uint32_t queue_count = 0;
    for (uint32_t family = 0; family < info.queue_family_count; ++family) {
        nisyros_queue_family_info queue;
        assert(nisyros_cluster_queue_family_info(cluster, ids[0], family, &queue) ==
               NISYROS_SUCCESS);
        assert(queue.family == family);
        assert(queue.engine_identity_known == 0);
        assert(queue.affinity_enforceable == 0);
        if ((queue.flags & NISYROS_QUEUE_COMPUTE) != 0 && queue.count > queue_count) {
            selected = family;
            queue_count = queue.count;
        }
    }
    assert(selected != UINT32_MAX);

    nisyros_queue_request request = {selected, queue_count < 2 ? 1 : 2, 1.0f, 0};
    nisyros_device_open_desc desc = {
        .header = {sizeof(desc), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    nisyros_device device = NULL;
    nisyros_status status = nisyros_device_open(cluster, ids[0], &desc, &device);
    if (expect_strict) {
        assert(status == NISYROS_SUCCESS);
        assert(device != NULL);
        assert(nisyros_cluster_close(cluster) == NISYROS_BUSY);
        for (uint32_t index = 0; index < request.count; ++index) {
            nisyros_queue queue = NULL;
            assert(nisyros_queue_get(device, selected, index, &queue) == NISYROS_SUCCESS);
            assert(queue != NULL);
        }
        nisyros_alloc_desc allocation = {
            .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
            .bytes = 64,
            .alignment = 256,
            .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
            .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
        };
        nisyros_addr address = 0;
        assert(nisyros_alloc(device, &allocation, &address) == NISYROS_SUCCESS);
        assert(address != 0 && address % 256 == 0);
        assert(nisyros_device_close(device) == NISYROS_BUSY);
        void *host = NULL;
        assert(nisyros_map(device, address + 4, 4, &host) == NISYROS_SUCCESS);
        assert(host != NULL);
        memset(host, 0xa5, 4);
        assert(nisyros_flush(device, address + 4, 4) == NISYROS_SUCCESS);
        assert(nisyros_invalidate(device, address + 4, 4) == NISYROS_SUCCESS);
        void *mapped = host;
        assert(nisyros_map(device, address + 63, 2, &host) == NISYROS_INVALID_ARGUMENT);
        assert(host == NULL);
        assert(nisyros_free(device, address) == NISYROS_BUSY);
        assert(nisyros_map(device, address + 4, 4, &host) == NISYROS_INVALID_ARGUMENT);
        assert(host == NULL);
        assert(nisyros_free(device, address + 4) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_unmap(device, (void *)(uintptr_t)1) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_unmap(device, mapped) == NISYROS_SUCCESS);
        assert(nisyros_free(device, address) == NISYROS_SUCCESS);

        nisyros_addr source = 0, destination = 0;
        assert(nisyros_alloc(device, &allocation, &source) == NISYROS_SUCCESS);
        assert(nisyros_alloc(device, &allocation, &destination) == NISYROS_SUCCESS);
        void *source_host = NULL, *destination_host = NULL;
        assert(nisyros_map(device, source, 64, &source_host) == NISYROS_SUCCESS);
        assert(nisyros_map(device, destination, 64, &destination_host) == NISYROS_SUCCESS);
        memset(source_host, 0x3c, 64);
        memset(destination_host, 0, 64);
        assert(nisyros_flush(device, source, 64) == NISYROS_SUCCESS);
        assert(nisyros_flush(device, destination, 64) == NISYROS_SUCCESS);

        nisyros_cmd cmd = NULL;
        assert(nisyros_cmd_create(device, selected, &cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(cmd, destination + 1, 4, 0) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_cmd_end(cmd) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_cmd_reset(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy(cmd, destination + 8, source + 4, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_barrier_range(cmd, destination + 8, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_barrier_all(cmd) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(cmd, destination + 24, 8, 0x01020304) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(cmd) == NISYROS_SUCCESS);
        assert(nisyros_free(device, source) == NISYROS_BUSY);
        nisyros_queue queue = NULL;
        assert(nisyros_queue_get(device, selected, 0, &queue) == NISYROS_SUCCESS);
        for (uint64_t iteration = 1; iteration <= 2; ++iteration) {
            nisyros_point done = {0};
            assert(nisyros_submit(queue, cmd, NULL, &done) == NISYROS_SUCCESS);
            assert(done.event != NULL && done.value == iteration);
            nisyros_status poll = nisyros_wait(done, 0);
            assert(poll == NISYROS_SUCCESS || poll == NISYROS_TIMEOUT);
            assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            uint64_t completed = 0;
            assert(nisyros_event_query(done.event, &completed) == NISYROS_SUCCESS);
            assert(completed >= iteration);
        }
        assert(nisyros_invalidate(device, destination, 64) == NISYROS_SUCCESS);
        for (int i = 8; i < 24; ++i) assert(((unsigned char *)destination_host)[i] == 0x3c);
        uint32_t pattern[2] = {0x01020304, 0x01020304};
        assert(memcmp((unsigned char *)destination_host + 24, pattern, sizeof(pattern)) == 0);
        assert(nisyros_cmd_destroy(cmd) == NISYROS_SUCCESS);
        if (request.count >= 2) {
            nisyros_event_desc event_desc = {
                .header = {sizeof(event_desc), NISYROS_ABI_VERSION, NULL},
                .signaling_domain = NISYROS_SIGNAL_DEVICE,
            };
            nisyros_event ready = NULL;
            assert(nisyros_event_create(device, &event_desc, &ready) == NISYROS_SUCCESS);
            nisyros_point ready_one = {ready, 1};
            nisyros_cmd producer = NULL, consumer = NULL, unordered = NULL;
            assert(nisyros_cmd_create(device, selected, &producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(producer, source + 32, 16, 0x55667788) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(producer, ready_one) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &consumer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(consumer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(consumer, ready_one) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy(consumer, destination + 32, source + 32, 16) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(consumer) == NISYROS_SUCCESS);
            nisyros_queue queue_two = NULL;
            assert(nisyros_queue_get(device, selected, 1, &queue_two) == NISYROS_SUCCESS);
            nisyros_point consumer_done = {0}, producer_done = {0};
            assert(nisyros_submit(queue_two, consumer, NULL, &consumer_done) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue, producer, NULL, &producer_done) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(ready) == NISYROS_BUSY);

            assert(nisyros_cmd_create(device, selected, &unordered) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(unordered) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(unordered, (nisyros_point){ready, 2}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(unordered) == NISYROS_SUCCESS);
            nisyros_point rejected = {0};
            assert(nisyros_submit(queue_two, unordered, NULL, &rejected) == NISYROS_INVALID_ARGUMENT);
            assert(rejected.event == NULL);
            assert(nisyros_cmd_destroy(unordered) == NISYROS_SUCCESS);

            assert(nisyros_wait(consumer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            assert(nisyros_wait(producer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            assert(nisyros_invalidate(device, destination + 32, 16) == NISYROS_SUCCESS);
            uint32_t cross_queue_pattern[4] = {0x55667788, 0x55667788, 0x55667788, 0x55667788};
            assert(memcmp((unsigned char *)destination_host + 32, cross_queue_pattern,
                          sizeof(cross_queue_pattern)) == 0);
            assert(nisyros_cmd_destroy(producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(consumer) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(ready) == NISYROS_SUCCESS);

            nisyros_event left = NULL, right = NULL;
            assert(nisyros_event_create(device, &event_desc, &left) == NISYROS_SUCCESS);
            assert(nisyros_event_create(device, &event_desc, &right) == NISYROS_SUCCESS);
            nisyros_cmd left_producer = NULL, right_producer = NULL;
            nisyros_cmd joined = NULL, fanout = NULL;
            assert(nisyros_cmd_create(device, selected, &left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(left_producer, source, 16, 0x11111111) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(left_producer, (nisyros_point){left, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(right_producer, source + 16, 16, 0x22222222) == NISYROS_SUCCESS);
            assert(nisyros_cmd_signal(right_producer, (nisyros_point){right, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(joined, (nisyros_point){left, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(joined, (nisyros_point){right, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy(joined, destination, source, 32) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_create(device, selected, &fanout) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(fanout) == NISYROS_SUCCESS);
            assert(nisyros_cmd_wait(fanout, (nisyros_point){left, 1}) == NISYROS_SUCCESS);
            assert(nisyros_cmd_fill(fanout, destination + 56, 4, 0x33333333) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(fanout) == NISYROS_SUCCESS);
            nisyros_point ignored = {0}, joined_done = {0}, fanout_done = {0};
            assert(nisyros_submit(queue, left_producer, NULL, &ignored) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue_two, right_producer, NULL, &ignored) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue, joined, NULL, &joined_done) == NISYROS_SUCCESS);
            assert(nisyros_submit(queue_two, fanout, NULL, &fanout_done) == NISYROS_SUCCESS);
            nisyros_point both_done[2] = {joined_done, fanout_done};
            assert(nisyros_wait_many(2, both_done, NISYROS_WAIT_ALL,
                                     NISYROS_TIMEOUT_INFINITE, NULL) == NISYROS_SUCCESS);
            assert(nisyros_invalidate(device, destination, 64) == NISYROS_SUCCESS);
            for (int i = 0; i < 16; ++i) assert(((unsigned char *)destination_host)[i] == 0x11);
            for (int i = 16; i < 32; ++i) assert(((unsigned char *)destination_host)[i] == 0x22);
            assert(*(uint32_t *)((unsigned char *)destination_host + 56) == 0x33333333);
            assert(nisyros_cmd_destroy(left_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(right_producer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(joined) == NISYROS_SUCCESS);
            assert(nisyros_cmd_destroy(fanout) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(left) == NISYROS_SUCCESS);
            assert(nisyros_event_destroy(right) == NISYROS_SUCCESS);
        }
        nisyros_event_desc replay_event_desc = {
            .header = {sizeof(replay_event_desc), NISYROS_ABI_VERSION, NULL},
            .signaling_domain = NISYROS_SIGNAL_DEVICE,
        };
        nisyros_event input_ready = NULL, output_ready = NULL;
        assert(nisyros_event_create(device, &replay_event_desc, &input_ready) == NISYROS_SUCCESS);
        assert(nisyros_event_create(device, &replay_event_desc, &output_ready) == NISYROS_SUCCESS);
        nisyros_cmd prepare = NULL, replay = NULL;
        assert(nisyros_cmd_create(device, selected, &prepare) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(prepare) == NISYROS_SUCCESS);
        assert(nisyros_cmd_signal(prepare, (nisyros_point){input_ready, 1}) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(prepare) == NISYROS_SUCCESS);
        nisyros_point prepare_done = {0};
        assert(nisyros_submit(queue, prepare, NULL, &prepare_done) == NISYROS_SUCCESS);
        assert(nisyros_wait(prepare_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);

        assert(nisyros_cmd_create(device, selected, &replay) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(replay) == NISYROS_SUCCESS);
        assert(nisyros_cmd_wait_slot(replay, 0) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(replay, destination + 48, 16, 0xdeadbeef) == NISYROS_SUCCESS);
        assert(nisyros_cmd_signal_slot(replay, 1) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(replay) == NISYROS_SUCCESS);
        nisyros_cmd_info replay_info = {
            .header = {sizeof(replay_info), NISYROS_ABI_VERSION, NULL},
        };
        assert(nisyros_cmd_query(replay, &replay_info) == NISYROS_SUCCESS);
        assert(replay_info.native_segment_count == 1);
        assert(replay_info.native_record_count == 1);
        assert(replay_info.requires_host_progress == 0);
        nisyros_point replay_done = {0};
        assert(nisyros_submit(queue, replay, NULL, &replay_done) == NISYROS_INVALID_ARGUMENT);
        assert(replay_done.event == NULL);
        for (uint64_t iteration = 1; iteration <= 2; ++iteration) {
            nisyros_event_binding bindings[2] = {
                {0, 0, {input_ready, 1}},
                {1, 0, {output_ready, iteration}},
            };
            nisyros_submit_desc submit_desc = {
                .header = {sizeof(submit_desc), NISYROS_ABI_VERSION, NULL},
                .binding_count = 2,
                .bindings = bindings,
            };
            assert(nisyros_submit(queue, replay, &submit_desc, &replay_done) == NISYROS_SUCCESS);
            assert(nisyros_wait(replay_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            assert(nisyros_wait((nisyros_point){output_ready, iteration}, 0) == NISYROS_SUCCESS);
        }
        assert(nisyros_cmd_query(replay, &replay_info) == NISYROS_SUCCESS);
        assert(replay_info.native_record_count == 1);
        assert(nisyros_invalidate(device, destination + 48, 16) == NISYROS_SUCCESS);
        uint32_t replay_pattern[4] = {0xdeadbeef, 0xdeadbeef, 0xdeadbeef, 0xdeadbeef};
        assert(memcmp((unsigned char *)destination_host + 48, replay_pattern,
                      sizeof(replay_pattern)) == 0);
        assert(nisyros_cmd_destroy(prepare) == NISYROS_SUCCESS);
        assert(nisyros_cmd_destroy(replay) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(input_ready) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(output_ready) == NISYROS_SUCCESS);

        nisyros_event_desc host_event_desc = {
            .header = {sizeof(host_event_desc), NISYROS_ABI_VERSION, NULL},
            .signaling_domain = NISYROS_SIGNAL_HOST,
        };
        nisyros_event host_event = NULL;
        assert(nisyros_event_create(device, &host_event_desc, &host_event) == NISYROS_SUCCESS);
        nisyros_point host_one = {host_event, 1};
        assert(nisyros_wait(host_one, 0) == NISYROS_TIMEOUT);
        assert(nisyros_event_host_signal(host_event, 1) == NISYROS_SUCCESS);
        assert(nisyros_event_host_signal(host_event, 1) == NISYROS_INVALID_ARGUMENT);
        assert(nisyros_wait(host_one, 0) == NISYROS_SUCCESS);
        nisyros_point two_points[2] = {{host_event, 1}, {host_event, 2}};
        uint32_t satisfied = UINT32_MAX;
        assert(nisyros_wait_many(2, two_points, NISYROS_WAIT_ANY, 0, &satisfied) == NISYROS_SUCCESS);
        assert(satisfied == 0);
        assert(nisyros_wait_many(2, two_points, NISYROS_WAIT_ALL, 0, NULL) == NISYROS_TIMEOUT);
        nisyros_cmd host_consumer = NULL;
        assert(nisyros_cmd_create(device, selected, &host_consumer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(host_consumer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_wait(host_consumer, two_points[1]) == NISYROS_SUCCESS);
        assert(nisyros_cmd_fill(host_consumer, destination + 40, 4, 0x12345678) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(host_consumer) == NISYROS_SUCCESS);
        nisyros_point host_consumer_done = {0};
        assert(nisyros_submit(queue, host_consumer, NULL, &host_consumer_done) == NISYROS_SUCCESS);
        assert(nisyros_event_host_signal(host_event, 2) == NISYROS_SUCCESS);
        assert(nisyros_wait_many(2, two_points, NISYROS_WAIT_ALL,
                                 NISYROS_TIMEOUT_INFINITE, NULL) == NISYROS_SUCCESS);
        assert(nisyros_wait(host_consumer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        assert(nisyros_invalidate(device, destination + 40, 4) == NISYROS_SUCCESS);
        assert(*(uint32_t *)((unsigned char *)destination_host + 40) == 0x12345678);
        assert(nisyros_cmd_destroy(host_consumer) == NISYROS_SUCCESS);
        assert(nisyros_event_destroy(host_event) == NISYROS_SUCCESS);

        nisyros_host_alloc_desc host_desc = {
            .header = {sizeof(host_desc), NISYROS_ABI_VERSION, NULL},
            .bytes = 64,
            .alignment = 64,
        };
        void *upload = NULL, *download = NULL;
        assert(nisyros_host_alloc(device, &host_desc, &upload) == NISYROS_SUCCESS);
        assert(nisyros_host_alloc(device, &host_desc, &download) == NISYROS_SUCCESS);
        memset(upload, 0x5a, 64);
        memset(download, 0, 64);
        nisyros_cmd transfer = NULL;
        assert(nisyros_cmd_create(device, selected, &transfer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_begin(transfer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy_h2d(transfer, source, (unsigned char *)upload + 4, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_barrier_all(transfer) == NISYROS_SUCCESS);
        assert(nisyros_cmd_copy_d2h(transfer, (unsigned char *)download + 8, source, 16) == NISYROS_SUCCESS);
        assert(nisyros_cmd_end(transfer) == NISYROS_SUCCESS);
        assert(nisyros_host_free(device, upload) == NISYROS_BUSY);
        nisyros_point transfer_done = {0};
        assert(nisyros_submit(queue, transfer, NULL, &transfer_done) == NISYROS_SUCCESS);
        assert(nisyros_wait(transfer_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        for (int i = 8; i < 24; ++i) assert(((unsigned char *)download)[i] == 0x5a);
        assert(nisyros_cmd_destroy(transfer) == NISYROS_SUCCESS);
        assert(nisyros_host_free(device, upload) == NISYROS_SUCCESS);
        assert(nisyros_host_free(device, download) == NISYROS_SUCCESS);

        uint64_t registration_alignment = 0;
        if (nisyros_host_registration_alignment(device, &registration_alignment) == NISYROS_SUCCESS) {
            assert(registration_alignment >= sizeof(void *));
            assert((registration_alignment & (registration_alignment - 1)) == 0);
            void *registered_upload = aligned_alloc((size_t)registration_alignment,
                                                    (size_t)registration_alignment);
            void *registered_download = aligned_alloc((size_t)registration_alignment,
                                                      (size_t)registration_alignment);
            assert(registered_upload != NULL && registered_download != NULL);
            memset(registered_upload, 0x69, (size_t)registration_alignment);
            memset(registered_download, 0, (size_t)registration_alignment);
            assert(nisyros_host_register(device, registered_upload, registration_alignment) == NISYROS_SUCCESS);
            assert(nisyros_host_register(device, registered_download, registration_alignment) == NISYROS_SUCCESS);
            nisyros_cmd registered_transfer = NULL;
            assert(nisyros_cmd_create(device, selected, &registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_begin(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy_h2d(registered_transfer, source, registered_upload, 16) == NISYROS_SUCCESS);
            assert(nisyros_cmd_barrier_all(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_cmd_copy_d2h(registered_transfer, registered_download, source, 16) == NISYROS_SUCCESS);
            assert(nisyros_cmd_end(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_host_unregister(device, registered_upload) == NISYROS_BUSY);
            nisyros_point registered_done = {0};
            assert(nisyros_submit(queue, registered_transfer, NULL, &registered_done) == NISYROS_SUCCESS);
            assert(nisyros_wait(registered_done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
            for (int i = 0; i < 16; ++i) assert(((unsigned char *)registered_download)[i] == 0x69);
            assert(nisyros_cmd_destroy(registered_transfer) == NISYROS_SUCCESS);
            assert(nisyros_host_unregister(device, registered_upload) == NISYROS_SUCCESS);
            assert(nisyros_host_unregister(device, registered_download) == NISYROS_SUCCESS);
            free(registered_upload);
            free(registered_download);
            printf("registered host range verified (alignment=%llu)\n",
                   (unsigned long long)registration_alignment);
        }
        assert(nisyros_unmap(device, source_host) == NISYROS_SUCCESS);
        assert(nisyros_unmap(device, destination_host) == NISYROS_SUCCESS);
        assert(nisyros_free(device, source) == NISYROS_SUCCESS);
        assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
        assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    } else {
        assert(status == NISYROS_UNSUPPORTED);
        assert(device == NULL);
    }
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    return 0;
}
