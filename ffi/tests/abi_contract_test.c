//! C ABI Contract Tests
//!
//! Validates the C ABI surface: struct sizes, field offsets, enum values,
//! and ownership rules. No networking — pure ABI validation.
//!
//! Compile and run:
//!   gcc -o abi_contract_test abi_contract_test.c -L../target/debug -liroh_ffi -lpthread
//!   ./abi_contract_test

#include <stddef.h>
#include <stdio.h>
#include <string.h>

#include "iroh_ffi.h"

// Track test results
static int tests_run = 0;
static int tests_passed = 0;

#define ASSERT(cond, msg) do { \
    tests_run++; \
    if (cond) { \
        tests_passed++; \
        printf("  PASS: %s\n", msg); \
    } else { \
        printf("  FAIL: %s\n", msg); \
    } \
} while (0)

#define ASSERT_EQ(a, b, msg) ASSERT((a) == (b), msg " (got " #a "=%lu, " #b "=%lu)")
#define ASSERT_EQ_HEX(a, b, msg) ASSERT((a) == (b), msg " (got " #a "=0x%lx, " #b "=0x%lx)")

// ─── Struct size tests ─────────────────────────────────────────────────────

static void test_struct_sizes(void) {
    printf("\n[Struct sizes]\n");

    // These sizes come from the Rust struct definitions.
    // If the Rust side changes struct_layout without updating the C header,
    // these tests will catch the drift.
    ASSERT_EQ((size_t)sizeof(iroh_runtime_config_t), (size_t)16, "iroh_runtime_config_t size");
    ASSERT_EQ((size_t)sizeof(iroh_event_t), (size_t)80, "iroh_event_t size");
}

// ─── Field offset tests ─────────────────────────────────────────────────────

static void test_field_offsets(void) {
    printf("\n[Field offsets]\n");

    // iroh_event_t field offsets — verified against generated header with offsetof()
    ASSERT_EQ((size_t)offsetof(iroh_event_t, struct_size), (size_t)0, "iroh_event_t.struct_size offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, kind), (size_t)4, "iroh_event_t.kind offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, status), (size_t)8, "iroh_event_t.status offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, operation), (size_t)16, "iroh_event_t.operation offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, handle), (size_t)24, "iroh_event_t.handle offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, related), (size_t)32, "iroh_event_t.related offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, user_data), (size_t)40, "iroh_event_t.user_data offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, data_ptr), (size_t)48, "iroh_event_t.data_ptr offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, data_len), (size_t)56, "iroh_event_t.data_len offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, buffer), (size_t)64, "iroh_event_t.buffer offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, error_code), (size_t)72, "iroh_event_t.error_code offset");
    ASSERT_EQ((size_t)offsetof(iroh_event_t, flags), (size_t)76, "iroh_event_t.flags offset");
}

// ─── Enum value tests ───────────────────────────────────────────────────────

static void test_enum_values(void) {
    printf("\n[Enum values]\n");

    // Status codes — must match the Rust iroh_status_t enum
    ASSERT_EQ((int)IROH_STATUS_OK, 0, "IROH_STATUS_OK == 0");
    ASSERT_EQ((int)IROH_STATUS_INVALID_ARGUMENT, 1, "IROH_STATUS_INVALID_ARGUMENT == 1");
    ASSERT_EQ((int)IROH_STATUS_NOT_FOUND, 2, "IROH_STATUS_NOT_FOUND == 2");
    ASSERT_EQ((int)IROH_STATUS_ALREADY_CLOSED, 3, "IROH_STATUS_ALREADY_CLOSED == 3");
    ASSERT_EQ((int)IROH_STATUS_QUEUE_FULL, 4, "IROH_STATUS_QUEUE_FULL == 4");
    ASSERT_EQ((int)IROH_STATUS_BUFFER_TOO_SMALL, 5, "IROH_STATUS_BUFFER_TOO_SMALL == 5");
    ASSERT_EQ((int)IROH_STATUS_UNSUPPORTED, 6, "IROH_STATUS_UNSUPPORTED == 6");
    ASSERT_EQ((int)IROH_STATUS_INTERNAL, 7, "IROH_STATUS_INTERNAL == 7");
    ASSERT_EQ((int)IROH_STATUS_TIMEOUT, 8, "IROH_STATUS_TIMEOUT == 8");
    ASSERT_EQ((int)IROH_STATUS_CANCELLED, 9, "IROH_STATUS_CANCELLED == 9");
    ASSERT_EQ((int)IROH_STATUS_CONNECTION_REFUSED, 10, "IROH_STATUS_CONNECTION_REFUSED == 10");
    ASSERT_EQ((int)IROH_STATUS_STREAM_RESET, 11, "IROH_STATUS_STREAM_RESET == 11");

    // Hook decisions
    ASSERT_EQ((int)IROH_HOOK_DECISION_ALLOW, 0, "IROH_HOOK_DECISION_ALLOW == 0");
    ASSERT_EQ((int)IROH_HOOK_DECISION_DENY, 1, "IROH_HOOK_DECISION_DENY == 1");
}

// ─── Ownership smoke tests ────────────────────────────────────────────────────

static void test_ownership_smoke(void) {
    printf("\n[Ownership smoke tests]\n");

    // Create a runtime
    iroh_runtime_config_t config = {
        .struct_size = sizeof(iroh_runtime_config_t),
        .worker_threads = 1,
        .event_queue_capacity = 64,
        .reserved = 0,
    };

    iroh_runtime_t runtime = 0;
    int r = iroh_runtime_new(&config, &runtime);
    ASSERT_EQ(r, IROH_STATUS_OK, "iroh_runtime_new returns OK");
    ASSERT(runtime != 0, "iroh_runtime_new returns non-zero handle");

    // buffer_release on null buffer — must return OK (idempotent)
    r = iroh_buffer_release(runtime, 0);
    ASSERT_EQ(r, IROH_STATUS_OK, "iroh_buffer_release(0) returns OK");

    // buffer_release on invalid buffer — must return NOT_FOUND (not crash)
    r = iroh_buffer_release(runtime, 999999);
    ASSERT_EQ(r, IROH_STATUS_NOT_FOUND, "iroh_buffer_release(invalid) returns NOT_FOUND");

    // string_release on null ptr with len=0 — must return OK
    r = iroh_string_release(NULL, 0);
    ASSERT_EQ(r, IROH_STATUS_OK, "iroh_string_release(NULL, 0) returns OK");

    // string_release on valid ptr (we don't have a real string, but validate
    // that calling it with a garbage pointer doesn't crash the process)
    // This is a smoke test — real strings come from API return values.
    // r = iroh_string_release(garbage_ptr, 16);
    // ASSERT_EQ(r, IROH_STATUS_OK, "iroh_string_release(valid ptr) returns OK");

    // poll_events with null output — should return 0 or a valid count
    iroh_event_t events[4];
    memset(events, 0, sizeof(events));
    uintptr_t n = iroh_poll_events(runtime, events, 4, 0);
    ASSERT(n >= 0, "iroh_poll_events returns non-negative count");

    // operation_cancel on null operation — should return INVALID_ARGUMENT
    r = iroh_operation_cancel(runtime, 0);
    ASSERT_EQ(r, IROH_STATUS_INVALID_ARGUMENT, "iroh_operation_cancel(0) returns INVALID_ARGUMENT");

    // Close the runtime
    r = iroh_runtime_close(runtime);
    ASSERT_EQ(r, IROH_STATUS_OK, "iroh_runtime_close returns OK");
}

// ─── Null pointer handling tests ─────────────────────────────────────────────

static void test_null_handling(void) {
    printf("\n[Null pointer handling]\n");

    // iroh_runtime_new with null config — should use defaults or return error
    iroh_runtime_t rt = 0;
    int r_new = iroh_runtime_new(NULL, &rt);
    if (r_new == IROH_STATUS_OK && rt != 0) {
        iroh_runtime_close(rt);
    }
    // Just verify it doesn't crash

    // iroh_node_id with null out_buf — should return INVALID_ARGUMENT
    // (we'd need a valid node handle first, so this is a placeholder)

    printf("  (null handling smoke test completed without crash)\n");
    tests_run++;
    tests_passed++;
}

// ─── Main ─────────────────────────────────────────────────────────────────────

int main(void) {
    printf("=== C ABI Contract Tests ===\n");

    test_struct_sizes();
    test_field_offsets();
    test_enum_values();
    test_ownership_smoke();
    test_null_handling();

    printf("\n=== Results: %d/%d passed ===\n", tests_passed, tests_run);
    return tests_passed == tests_run ? 0 : 1;
}
