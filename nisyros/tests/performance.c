#define _POSIX_C_SOURCE 200809L
#include "nisyros.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

enum { ITERATIONS = 1000, TRANSFER_ITERATIONS = 20 };
static const uint64_t TRANSFER_BYTES = UINT64_C(64) * 1024 * 1024;

static uint64_t now_ns(clockid_t clock) {
    struct timespec time;
    assert(clock_gettime(clock, &time) == 0);
    return (uint64_t)time.tv_sec * UINT64_C(1000000000) + (uint64_t)time.tv_nsec;
}

static void record(FILE *file, const char *metric, uint32_t iteration,
                   uint64_t wall_ns, uint64_t cpu_ns, uint64_t bytes,
                   const nisyros_cmd_info *info) {
    assert(fprintf(file, "%s,%u,%llu,%llu,%llu,%u,%llu\n", metric, iteration,
                   (unsigned long long)wall_ns, (unsigned long long)cpu_ns,
                   (unsigned long long)bytes, info->native_segment_count,
                   (unsigned long long)info->native_record_count) > 0);
}

static nisyros_cmd make_command(nisyros_device device, uint32_t family) {
    nisyros_cmd command = NULL;
    assert(nisyros_cmd_create(device, family, &command) == NISYROS_SUCCESS);
    assert(nisyros_cmd_begin(command) == NISYROS_SUCCESS);
    return command;
}

static nisyros_cmd_info command_info(nisyros_cmd command) {
    nisyros_cmd_info info = {.header = {sizeof(info), NISYROS_ABI_VERSION, NULL}};
    assert(nisyros_cmd_query(command, &info) == NISYROS_SUCCESS);
    return info;
}

int main(int argc, char **argv) {
    assert(argc == 2);
    FILE *raw = fopen(argv[1], "w");
    assert(raw != NULL);
    nisyros_cluster cluster = NULL;
    assert(nisyros_cluster_open_local(&cluster) == NISYROS_SUCCESS);
    uint32_t count = 0;
    assert(nisyros_cluster_devices(cluster, &count, NULL) == NISYROS_SUCCESS);
    assert(count > 0 && count <= 32);
    nisyros_device_id ids[32];
    uint32_t capacity = 32;
    assert(nisyros_cluster_devices(cluster, &capacity, ids) == NISYROS_SUCCESS);
    uint32_t selected = UINT32_MAX;
    nisyros_device_info identity = {.header = {sizeof(identity), NISYROS_ABI_VERSION, NULL}};
    for (uint32_t index = 0; index < count; ++index) {
        nisyros_device_info candidate = {.header = {sizeof(candidate), NISYROS_ABI_VERSION, NULL}};
        assert(nisyros_cluster_device_info(cluster, ids[index], &candidate) == NISYROS_SUCCESS);
        if (candidate.strict_profile_supported && candidate.device_type != NISYROS_DEVICE_TYPE_CPU) {
            selected = index;
            identity = candidate;
            break;
        }
    }
    if (selected == UINT32_MAX) {
        assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
        assert(fclose(raw) == 0);
        return 77;
    }
    uint32_t family = UINT32_MAX;
    for (uint32_t index = 0; index < identity.queue_family_count; ++index) {
        nisyros_queue_family_info queue_info;
        assert(nisyros_cluster_queue_family_info(cluster, ids[selected], index,
                                                 &queue_info) == NISYROS_SUCCESS);
        if ((queue_info.flags & (NISYROS_QUEUE_COMPUTE | NISYROS_QUEUE_TRANSFER)) ==
            (NISYROS_QUEUE_COMPUTE | NISYROS_QUEUE_TRANSFER)) { family = index; break; }
    }
    assert(family != UINT32_MAX);
    nisyros_queue_request request = {family, 1, 1.0f, 0};
    nisyros_device_open_desc open = {
        .header = {sizeof(open), NISYROS_ABI_VERSION, NULL},
        .queue_request_count = 1,
        .queue_requests = &request,
    };
    nisyros_device device = NULL;
    assert(nisyros_device_open(cluster, ids[selected], &open, &device) == NISYROS_SUCCESS);
    nisyros_queue queue = NULL;
    assert(nisyros_queue_get(device, family, 0, &queue) == NISYROS_SUCCESS);
    assert(fprintf(raw, "# device=%s driver=%s vendor=%u device=%u api=%u\n",
                   identity.name, identity.driver_name, identity.vendor_id,
                   identity.device_id, identity.api_version) > 0);
    assert(fprintf(raw, "# driver_id=%u device_uuid=", identity.driver_id) > 0);
    for (int i = 0; i < 16; ++i) assert(fprintf(raw, "%02x", identity.device_uuid[i]) > 0);
    assert(fprintf(raw, " driver_uuid=") > 0);
    for (int i = 0; i < 16; ++i) assert(fprintf(raw, "%02x", identity.driver_uuid[i]) > 0);
    assert(fprintf(raw, "\n") > 0);
    assert(fprintf(raw, "metric,iteration,wall_ns,cpu_ns,bytes,segments,native_record_count\n") > 0);

    nisyros_cmd empty = make_command(device, family);
    assert(nisyros_cmd_end(empty) == NISYROS_SUCCESS);
    nisyros_cmd_info empty_info = command_info(empty);
    assert(empty_info.native_segment_count == 1 && empty_info.native_record_count == 1);
    for (uint32_t iteration = 1; iteration <= ITERATIONS; ++iteration) {
        uint64_t wall_start = now_ns(CLOCK_MONOTONIC_RAW);
        uint64_t cpu_start = now_ns(CLOCK_THREAD_CPUTIME_ID);
        nisyros_point done = {0};
        assert(nisyros_submit(queue, empty, NULL, &done) == NISYROS_SUCCESS);
        uint64_t cpu_ns = now_ns(CLOCK_THREAD_CPUTIME_ID) - cpu_start;
        assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        record(raw, "empty", iteration, now_ns(CLOCK_MONOTONIC_RAW) - wall_start,
               cpu_ns, 0, &empty_info);
    }
    nisyros_cmd_info empty_after = command_info(empty);
    assert(empty_after.native_record_count == empty_info.native_record_count);
    assert(nisyros_cmd_destroy(empty) == NISYROS_SUCCESS);

    nisyros_event_desc event_desc = {
        .header = {sizeof(event_desc), NISYROS_ABI_VERSION, NULL},
        .signaling_domain = NISYROS_SIGNAL_DEVICE,
    };
    nisyros_event event = NULL;
    assert(nisyros_event_create(device, &event_desc, &event) == NISYROS_SUCCESS);
    nisyros_cmd segmented = make_command(device, family);
    assert(nisyros_cmd_signal_slot(segmented, 0) == NISYROS_SUCCESS);
    assert(nisyros_cmd_wait_slot(segmented, 0) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(segmented) == NISYROS_SUCCESS);
    nisyros_cmd_info segmented_info = command_info(segmented);
    assert(segmented_info.native_segment_count == 2 && segmented_info.native_record_count == 2);
    for (uint32_t iteration = 1; iteration <= ITERATIONS; ++iteration) {
        nisyros_event_binding binding = {0, 0, {event, iteration}};
        nisyros_submit_desc submit_desc = {
            .header = {sizeof(submit_desc), NISYROS_ABI_VERSION, NULL},
            .binding_count = 1,
            .bindings = &binding,
        };
        uint64_t wall_start = now_ns(CLOCK_MONOTONIC_RAW);
        uint64_t cpu_start = now_ns(CLOCK_THREAD_CPUTIME_ID);
        nisyros_point done = {0};
        assert(nisyros_submit(queue, segmented, &submit_desc, &done) == NISYROS_SUCCESS);
        uint64_t cpu_ns = now_ns(CLOCK_THREAD_CPUTIME_ID) - cpu_start;
        assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        record(raw, "segmented", iteration, now_ns(CLOCK_MONOTONIC_RAW) - wall_start,
               cpu_ns, 0, &segmented_info);
    }
    nisyros_cmd_info segmented_after = command_info(segmented);
    assert(segmented_after.native_record_count == segmented_info.native_record_count);
    assert(nisyros_cmd_destroy(segmented) == NISYROS_SUCCESS);
    assert(nisyros_event_destroy(event) == NISYROS_SUCCESS);

    nisyros_alloc_desc allocation = {
        .header = {sizeof(allocation), NISYROS_ABI_VERSION, NULL},
        .bytes = TRANSFER_BYTES,
        .alignment = 64,
        .required_properties = NISYROS_MEMORY_HOST_VISIBLE,
        .preferred_properties = NISYROS_MEMORY_HOST_COHERENT,
    };
    nisyros_addr source = 0, destination = 0;
    assert(nisyros_alloc(device, &allocation, &source) == NISYROS_SUCCESS);
    assert(nisyros_alloc(device, &allocation, &destination) == NISYROS_SUCCESS);
    uint8_t *source_host = NULL, *destination_host = NULL;
    assert(nisyros_map(device, source, TRANSFER_BYTES, (void **)&source_host) == NISYROS_SUCCESS);
    assert(nisyros_map(device, destination, TRANSFER_BYTES, (void **)&destination_host) == NISYROS_SUCCESS);
    for (uint64_t i = 0; i < TRANSFER_BYTES; ++i) source_host[i] = (uint8_t)(i * 17 + 31);
    memset(destination_host, 0, (size_t)TRANSFER_BYTES);
    assert(nisyros_flush(device, source, TRANSFER_BYTES) == NISYROS_SUCCESS);
    assert(nisyros_flush(device, destination, TRANSFER_BYTES) == NISYROS_SUCCESS);
    nisyros_cmd transfer = make_command(device, family);
    assert(nisyros_cmd_copy(transfer, destination, source, TRANSFER_BYTES) == NISYROS_SUCCESS);
    assert(nisyros_cmd_end(transfer) == NISYROS_SUCCESS);
    nisyros_cmd_info transfer_info = command_info(transfer);
    for (uint32_t iteration = 1; iteration <= TRANSFER_ITERATIONS; ++iteration) {
        uint64_t wall_start = now_ns(CLOCK_MONOTONIC_RAW);
        uint64_t cpu_start = now_ns(CLOCK_THREAD_CPUTIME_ID);
        nisyros_point done = {0};
        assert(nisyros_submit(queue, transfer, NULL, &done) == NISYROS_SUCCESS);
        uint64_t cpu_ns = now_ns(CLOCK_THREAD_CPUTIME_ID) - cpu_start;
        assert(nisyros_wait(done, NISYROS_TIMEOUT_INFINITE) == NISYROS_SUCCESS);
        record(raw, "transfer", iteration, now_ns(CLOCK_MONOTONIC_RAW) - wall_start,
               cpu_ns, TRANSFER_BYTES, &transfer_info);
    }
    assert(nisyros_invalidate(device, destination, TRANSFER_BYTES) == NISYROS_SUCCESS);
    assert(memcmp(source_host, destination_host, (size_t)TRANSFER_BYTES) == 0);
    nisyros_cmd_info transfer_after = command_info(transfer);
    assert(transfer_after.native_record_count == transfer_info.native_record_count);
    assert(nisyros_cmd_destroy(transfer) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, source_host) == NISYROS_SUCCESS);
    assert(nisyros_unmap(device, destination_host) == NISYROS_SUCCESS);
    assert(nisyros_free(device, source) == NISYROS_SUCCESS);
    assert(nisyros_free(device, destination) == NISYROS_SUCCESS);
    assert(nisyros_device_close(device) == NISYROS_SUCCESS);
    assert(nisyros_cluster_close(cluster) == NISYROS_SUCCESS);
    assert(fclose(raw) == 0);
    return 0;
}
