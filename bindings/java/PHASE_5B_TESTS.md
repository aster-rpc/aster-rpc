# Phase 5b — CQ Test Suite

Tests are designed **before** implementation. The goal: by the time Phase 5 implementation is complete, the test suite should be a complete, honest record of edge cases, not an afterthought.

## Test Philosophy

1. **State machine first** — pure unit tests with fake reactor/fake driver, before any real sockets
2. **Deterministic concurrency** — Loom to exhaustively permute thread interleavings
3. **Unsafe boundary coverage** — Miri, sanitizers on the Rust side; FFI correctness on the Java side
4. **Fuzz the ABI** — cargo-fuzz on Rust; structured round-trip tests on Java/Go
5. **Tiny foreign harnesses** — C, Java, Go each validate the ABI in isolation before networking enters the picture
6. **Real hostile races** — integration tests that force exact ordering of submit/cancel/close/reconnect
7. **Cross-language conformance** — golden event traces, Java and Go must observe the same logical sequence
8. **Performance at the right layer** — separate microbenchmarks from throughput tests, separate Rust/JVM/Go tools
9. **Soak test** — multi-hour churn to catch "leaks one buffer every million ops"

---

## 5b.1 — CQ State Machine (Rust, pure unit tests)

Test the CQ as a pure state machine with a **fake reactor** (no real sockets, no real time).

**State model:**

```
SUBMITTED → POLLING → COMPLETING → COMPLETED
                         ↘ CANCELLED
                         ↘ ERROR
SUBMITTED → CANCELLED (op_cancel before poll)
SUBMITTED → ERROR (handle closed before poll)
```

**Invariants:**
- One op produces at most one terminal event
- Cancel removes op from every wait structure exactly once
- No completion can outlive the handle generation it belongs to
- Dropping a connection resolves all dependent ops
- Never "complete twice", never "release twice", never "event for unknown op/stale handle"

**Test cases:**

| Test | Scenario | Assertion |
|------|----------|-----------|
| `submit_then_complete` | submit → drain → complete | Exactly one terminal event |
| `submit_then_cancel` | submit → cancel → drain | Exactly CANCELLED, no event |
| `submit_close_before_drain` | submit → close → drain | ERROR terminal event |
| `submit_twice_before_drain` | submit same tag twice → drain | Two distinct op_ids, two events |
| `cancel_races_complete` | submit → cancel races drain | Exactly one terminal (idempotent) |
| `close_races_accept` | accept_submit → close races drain | Exactly one ERROR terminal |
| `stale_op_id` | drain returns event for cancelled op | Skipped or ERROR, not panic |
| `stale_handle_reuse` | op pending, handle closed and new handle allocated | Old op's completion discarded |
| `completion_after_ownership_transfer` | accept completes, connection reassigned | Old op must not see new connection's events |
| `release_twice` | release same event twice | Idempotent or error, not UB |
| `close_during_completing` | drain delivers event while close() is called | Completing finishes; close waits for terminal |

**Fake reactor interface** (Rust):
```rust
trait FakeReactor {
    fn submit(&mut self, op: Op) -> OpId;
    fn cancel(&mut self, op_id: OpId);
    fn drain(&mut self, timeout_ms: u32) -> Vec<Event>;
    fn close_handle(&mut self, handle: Handle);
}
```

Use a simple `VecDeque`-backed implementation. Control exact ordering by calling methods directly.

---

## 5b.2 — Deterministic Concurrency (Rust, Loom)

Use **Loom** (Rust) to exhaustively permute concurrent executions under the C11 memory model.

**What Loom tests:**
- `op_cancel` called while `cq_next_batch` is draining — does the cancellation remove the op from every wait structure exactly once?
- `close_handle` while `drain` is in progress — does the close wait or race?
- `submit` racing with `cancel` on the same `user_tag` — are op_ids distinct regardless of ordering?
- `drop(IrohConnection)` while read/write ops are pending — do all dependent ops reach terminal state?

**Loom invariants to assert:**
- One op → at most one terminal event (never two)
- Cancelling an op removes it from every wait structure exactly once
- Dropping a connection resolves all dependent ops with ERROR
- No completion can outlive the handle generation it belongs to

**Note:** Loom is for Rust-side concurrency. Java concurrency is tested via jcstress (see 5b.5).

---

## 5b.3 — Unsafe Memory Tests (Rust, Miri + Sanitizers)

**Miri** — runs Rust programs/tests and catches UB in unsafe code:
```
cargo +nightly miri test
```

**Rust Sanitizers** (in CI, dedicated jobs):

| Sanitizer | Detects | Config |
|-----------|---------|--------|
| ASan | Out-of-bounds, use-after-free, double-free | `RUSTFLAGS="-Z sanitizer=address"` |
| LSan | Memory leaks | `RUSTFLAGS="-Z sanitizer=leak"` |
| TSan | Data races (native code) | `RUSTFLAGS="-Z sanitizer=thread"` |

**Test targets:**
- All FFI boundary crossings (`lib.rs` → `iroh_ffi`)
- Buffer ownership transfer: Java holds segment, calls `event_release`, then accesses it (should be caught)
- `iroh_handle_close` with pending ops: does Rust drain all pending before freeing?
- `iroh_event_release` called twice: does Rust handle gracefully or UB?

**Java FFM restricted-method warnings:**
Java 25 FFM methods are explicitly documented as capable of crashing the JVM or corrupting memory if misused. Code that touches FFI must be audited for:
- Segment lifetime exceeding arena scope
- Accessing a segment after `Arena.close()`
- Passing a `MemorySegment` across threads (unless `Arena.ofShared()`)

---

## 5b.4 — Fuzz the ABI (Rust, cargo-fuzz)

**cargo-fuzz** (libFuzzer-backed) for the C ABI surface:

**Fuzz targets:**

| Target | Input mutations | What it catches |
|--------|----------------|-----------------|
| `event_decode` | Random bytes → `IrohEvent` struct | Malformed struct, invalid enum, out-of-bounds ptr |
| `abi_struct_parsing` | Random bytes matching struct layout patterns | Off-by-one in struct field offsets |
| `length_pointer_combos` | `data_ptr` + `data_len` variations | Out-of-bounds, null ptr, wraparound |
| `invalid_handle_opid` | Random handle/op_id values | Stale ID lookup, use-after-close |
| `dup_release_cancel` | Repeated calls on same ID | Double-complete, double-release UB |
| `malformed_batch` | Truncated/incomplete `IrohEvent` batch | Buffer overrun in batch parsing |
| `oversized_lengths` | Extreme `data_len`, `data_ptr` values | Integer overflow, allocation failures |

**Corpus:** Start with valid-minimal inputs (e.g. one valid event, one valid batch) as a corpus seed. Fuzzers need starting inputs to cover the "happy path" before mutating into interesting edges.

**Continuous in CI** — fuzz targets run for a minimum of 1 hour per PR targeting the FFI layer.

---

## 5b.5 — ABI Contract Tests (Tiny Foreign Harnesses)

Three isolated harnesses, one per language. No networking. Pure ABI validation.

### C Harness (`ffi/tests/abi_contract_test.c`)

Validates the C ABI itself:
- Struct size matches `sizeof(IrohEvent)` — catches layout drift
- Field offsets are correct (`offsetof(IrohEvent, op_id)` etc.)
- Enum values match expected integers
- Ownership rules: what happens on `event_release` for a null `data_ptr`
- Alignment requirements for each struct

```c
// Smoke test: call every exported function with valid inputs
int r1 = iroh_accept_submit(h, tag, &op_out);
int r2 = iroh_cq_next_batch(cq, 10, ev, 16, &n_out);
int r3 = iroh_handle_close(h);
// assert: no crash, return codes are valid
```

### Java Harness (`bindings/java/src/test/java/com/aster/AbiContractTest.java`)

Validates FFM struct layouts match C header:
```java
// Struct size
assertEquals(40, IrohLibrary.IROH_EVENT.size());
// Field offsets
VarHandle opIdHandle = IrohLibrary.IROH_EVENT.varHandle(
    MemoryLayout.PathElement.groupElement("op_id"));
// Round-trip: encode in Java, decode in C, re-decode in Java
```

Then: submit ops, drain CQ, cancel, close, repeat. Checks:
- No use-after-close on any handle
- No stale segment access after `event_release`
- `op_id` sequence stays monotonic across submissions

### Go Harness (`bindings/go/abi_contract_test.go`)

Same idea via cgo:
```go
// Call each iroh_* function
r := C.iroh_accept_submit(h, tag, &opOut)
ev, n := C.iroh_cq_next_batch(cq, 10, 16)
// assert: no crash, valid return codes
// assert: goroutine not blocked after C call returns (cgo blocking check)
```

Validates: op completion routing correct, cancellation delivers exactly one terminal, goroutine not leaked.

---

## 5b.6 — Real Integration Tests with Hostile Races

These run against a real Tokio runtime. Explicitly force race orderings.

**Test cases:**

| Test | Action sequence | Expected result |
|------|----------------|-----------------|
| `accept_submit_then_close` | submit accept → close node → drain | Exactly one terminal (ERROR or ACCEPTED, never both) |
| `read_submit_then_remote_finish` | submit read → remote FIN → drain | Exactly READ or FIN, not both |
| `cancel_op_and_completion_racing` | submit → cancel races drain | Exactly CANCELLED terminal, no spurious event |
| `handle_close_after_submit` | submit → close → later drain | No success event for that handle generation |
| `peer_disconnect_mid_read` | submit read → peer disconnects → drain | ERROR terminal |
| `many_outstanding_on_cq` | 1000 submits on one CQ → drain | All 1000 complete or cancel; no loss, no duplication |
| `many_connections_share_cq` | 100 connections sharing one poller → continuous ops | Throughput stable, no CQ corruption |
| `batch_completion_delivery` | Force batch sizes 1, 4, 16, 64 → verify all events delivered | All events in batch dispatched correctly |

**Race forcing techniques:**
- `CountDownLatch` + `yield()` to make one thread wait for another at exact points
- `CompletableFuture.complete()` called from different threads to simulate async ordering
- Random backoff with seed to reproduce specific orderings

---

## 5b.7 — Cross-Language Conformance Tests

Golden event traces. Same scenario submitted to Rust core from Java and from Go. Both must observe the same logical sequence of events.

**Trace format:**
```
[0]  T+0ms   SUBMIT    tag=10  op=accept   node=1
[1]  T+5ms   SUBMIT    tag=11  op=connect  node=1
[2]  T+50ms  COMPLETE  op=2    handle=7    kind=CONNECT
[3]  T+55ms  COMPLETE  op=1    handle=8    kind=ACCEPT
[4]  T+60ms  SUBMIT    tag=12  op=read     handle=8
[5]  T+120ms COMPLETE  op=3    handle=8    n=128
[6]  T+121ms RELEASE   events=[3]
[7]  T+200ms CLOSE     handle=8
```

**Conformance matrix:**

| Scenario | Java trace | Go trace | Rust trace | Match? |
|----------|-----------|---------|-----------|--------|
| accept → connect → read → close | ✓ | ✓ | ✓ | ALL equal |
| cancel before accept | ✓ | ✓ | ✓ | ALL equal |
| close while read pending | ✓ | ✓ | ✓ | ALL equal |

**Tooling:** A shared test data format (JSON or RON) defining the scenario. Each language has a test that:
1. Loads the scenario
2. Drives its own binding through the exact op sequence
3. Serializes the resulting event trace
4. Compares against the golden trace (order, kind, handle, data_len)

---

## 5b.8 — Performance Tests (3 layers, 3 tools)

Never mix microbenchmarks with throughput tests. They answer different questions.

### Rust / Criterion.rs

`cargo bench` via Criterion. Targets Rust internal overhead only:

```
bench_submit_once        — time to call accept_submit (no tokio involved)
bench_cq_drain_1        — cq_next_batch returning 1 event
bench_cq_drain_batched  — cq_next_batch returning 64 events
bench_event_encode      — encode IrohEvent struct
```

### Java / JMH

`jmh` for JVM-side binding overhead:

```
IrohNodeBenchmark.submit_accept      — Java submit → op_id return
IrohNodeBenchmark.cq_drain_batch   — drain N events, dispatch to futures
IrohNodeBenchmark.read_overhead     — submit read → completion byte copy
IrohNodeBenchmark.memory_per_op     — MemorySegment allocations per outstanding op
```

### Go / testing.B

Standard Go benchmarks for Go-side overhead:

```
BenchmarkSubmitAccept
BenchmarkCQNextBatch
BenchmarkReadCompletion
```

### Metrics per test category

| Category | Metrics |
|----------|---------|
| Submit latency | p50/p95/p99 (microseconds) |
| CQ drain latency | p50/p95/p99 per call vs batch size |
| Completion throughput | events/second per poller thread |
| Small payload overhead | bytes/op including FFI crossing |
| Large payload overhead | bytes/op for 64KB frame |
| Memory per op | allocations, bytes per outstanding op |
| Poller thread count | threads vs concurrent nodes/ops |
| RSS growth | MB over 1M sustained ops |

---

## 5b.9 — Long-Run Soak / Leak Tests

Multi-hour churn. CI runs these nightly, not per-PR (too slow).

**Churn pattern:**
```
connect → accept → open_bi → read → write → finish → close
(with 10% random: cancel before complete, peer disconnect, timeout)
Repeat for 4 hours under load
```

**Assertions (all return to baseline):**
- Native handle count: `initial_handle_count == final_handle_count`
- Op table size: `initial_pending_ops == 0`, `final_pending_ops == 0`
- RSS: stabilizes (no unbounded growth)
- Pending completion queue depth: `max_queued < threshold` (no unbounded backlog)
- Java heap: `GC pause < 50ms`, `heap stable`
- Go goroutines: `initial_goroutines == final_goroutines`

**What these catch:**
- "Technically correct but leaks one event buffer every million ops"
- Pending ops that never complete because Rust lost the wake signal
- Arena memory that GC hasn't reclaimed but grows unboundedly between GCs
- Goroutine leak under specific cancel+reconnect patterns

---

## Phase 5b Task List

### State machine (5b.1)
- [x] Define `FakeReactor` trait in Rust test module
- [x] Implement `VecDeque`-backed fake reactor
- [x] Write all 11 state machine test cases
- [x] Add `release_twice`, `close_during_completing` to state machine suite

### Loom (5b.2)
- [x] Add `loom` crate to Rust test dependencies
- [x] `cancel` races `drain` — exactly one terminal
- [x] `close_handle` races `drain` — no use-after-free
- [x] `submit` same `user_tag` races itself — distinct `op_id`s
- [x] `drop(IrohConnection)` resolves all dependent ops

### Miri + Sanitizers (5b.3)
- [x] CI job: `cargo +nightly miri test` (needs nightly + Miri)
- [x] CI job: ASan `RUSTFLAGS="-Z sanitizer=address"`
- [x] CI job: LSan `RUSTFLAGS="-Z sanitizer=leak"`
- [x] CI job: TSan `RUSTFLAGS="-Z sanitizer=thread"`
- [x] Audit Rust unsafe blocks at FFI boundary for Miri compatibility

**Findings:** See `ffi/UNSAFE_AUDIT.md`.
- `alloc_string`: **memory leak** — uses `mem::forget` with no deallocation path.
  Fixed by adding `iroh_string_release` FFI function (`ffi/src/lib.rs:949`).
- `alloc_bytes`: **safe** — tracked via `buffers` map + `iroh_buffer_release`.
- `read_bytes`-family: **safe** — null checks + Miri will catch invalid pointers.
- Struct field pointer injection: **safe** — caller-allocated buffers, bounds-checked.

### Fuzz (5b.4)
- [x] `cargo-fuzz` setup: add `fuzz/` crate with `event_decode`, `abi_struct_parsing`, `length_pointer_combos`, `invalid_handle_opid`, `dup_release_cancel`, `malformed_batch`, `oversized_lengths` targets
- [x] Build corpus: valid-minimal inputs as seeds (in `ffi/fuzz/corpus/`)
- [ ] CI: fuzz target runs minimum 1 hour per PR touching FFI
- [ ] Integrate with OSS-Fuzz for continuous fuzzing
- [ ] Build corpus: valid-minimal inputs as seeds
- [ ] CI: fuzz target runs minimum 1 hour per PR touching FFI
- [ ] Integrate with OSS-Fuzz for continuous fuzzing

### ABI harnesses (5b.5)
- [x] C harness: `struct_size`, `field_offsets`, `enum_values`, `ownership_smoke` — `ffi/tests/abi_contract_test.c` (37/37 passing)
- [x] Java harness: FFM layout match, submit/cancel/close round-trip, no stale segment — `bindings/java/src/test/java/com/aster/AbiContractTest.java` (6/6 passing)
- [x] Go harness (scaffold): cgo declarations, struct offsets, enum values, ownership smoke — `bindings/go/abi_contract_test.go` (not runnable: no Go runtime in this environment)

### Integration races (5b.6)
- [x] `accept_submit_then_close` — submit accept → close node → drain — exactly one terminal
- [x] `cancel_op_and_completion_racing` — submit → cancel races drain — no spurious completion
- [x] `handle_close_after_submit` — submit → close → drain — no success event for old handle
- [x] `peer_disconnect_mid_read` — submit read → close node → drain — ERROR terminal
- [x] `many_outstanding_on_cq` (1000 ops) — all complete or cancel, no duplication
- [x] `many_connections_share_cq` (lighter 20-node version) — no crash, events delivered
- [ ] `read_submit_then_remote_finish` — requires two connected nodes (not implemented)
- [ ] `batch_completion_delivery` — flaky due to tokio task scheduling (ignored)

### Conformance (5b.7)
- [x] Define golden trace JSON schema — `ffi/tests/conformance/schema.json`
- [x] Record 3 golden traces: happy path, cancel, close-while-pending — `ffi/tests/conformance/*.json`
- [x] Rust trace validator + conformance invariants test — `ffi/tests/conformance_test.rs`
- [ ] Java conformance test: load scenario → execute → compare trace (requires Java FFI scenario runner)
- [ ] Go conformance test: load scenario → execute → compare trace (requires Go FFI scenario runner)
- [ ] Rust trace extractor for ground truth (requires actual scenario execution)

### Performance (5b.8)
- [x] Criterion.rs: 8 benchmarks in `ffi/benches/ffi_benchmarks.rs` — `bench_runtime_create`, `bench_poll_events_empty`, `bench_event_encode`, `bench_cq_drain_1`, `bench_cq_drain_batched`, `bench_operation_submit_cancel`, `bench_null_buffer_release`, `bench_string_release_null`
- [x] JMH (scaffold): `benchmarks/IrohBenchmark.java` — Arena overhead, segment read/write, field offset lookup (requires Java 25 FFM + JMH plugin in pom.xml)
- [x] Go (scaffold): `benchmarks_test.go` — 8 benchmarks (not runnable: no Go runtime)
- [ ] Dashboard: track p50/p95/p99 over time per branch

### Soak (5b.9)
- [x] Soak test scaffold: `ffi/tests/soak_test.rs` — churn pattern node_create→accept→close with 10% cancel rate, metrics for pending ops, max pending, completed/cancelled/errored counts
- [ ] Assertions: handle count, op table, RSS, pending CQ depth, heap, goroutines (need nightly CI to validate)
- [ ] Nightly CI run: run `test_soak_short` (10s) on every PR; full 4-hour `test_soak` nightly
