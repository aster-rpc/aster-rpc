# Java FFI / FFM Readiness Punch List for `lib.rs`

This document reviews the current C ABI bridge from the perspective of a Java FFI / FFM consumer.

## Overall assessment

The missing `lib.rs` materially improves the design. The project now has a real C ABI with:

- `#[no_mangle] extern "C"` exports
- `#[repr(C)]` structs
- opaque handles
- explicit free functions
- async operations represented by `OperationHandle`

That means the bridge is now **genuinely in Java-FFI territory**.

However, it is **not yet ready to bless as a production Java FFM interface**. The main remaining blockers are:

1. **in-flight handle lifetime safety**
2. **full custom relay support**
3. **better nonblocking operation support for Java async wrappers**
4. **clearer ownership and threading contracts**

---

## Status summary

### Already good

- Plain C ABI
- Opaque handles
- Explicit free functions
- Secret key input support
- Async native operations separated from Java language runtime concerns
- Reasonable overall surface for Java FFM wrapping

### Still missing or risky

- Raw pointer resurrection across async tasks
- No full custom relay URL support
- Blocking `operation_wait` as the main wait primitive
- No cancellation API
- No explicit operation kind query
- Ownership/threading rules not yet documented tightly enough
- Error model still too string-centric for robust Java mapping

---

# Priority 0: correctness and safety blockers

## 1. Eliminate raw-pointer resurrection in async tasks

### Problem

The current code repeatedly does this pattern:

1. convert `*mut T` to `usize`
2. spawn an async task
3. reconstruct `&T` from that integer later

This creates a **use-after-free risk** if Java frees the handle while the operation is still in flight.

### Why this is a blocker

A Java FFM consumer cannot be expected to perfectly coordinate native handle frees against all outstanding async work. If the ABI allows this, the result will be intermittent native crashes.

### Affected areas

Any function that:

- accepts a native handle
- spawns async work
- later dereferences the original handle

This includes connect, accept, open stream, read, write, close, and likely blobs/docs/gossip async operations too.

### Required fix

Refactor handles so that:

- each public handle wraps an `Arc<Inner>`
- async tasks clone the `Arc`
- free functions only drop one `Arc`

### Desired outcome

Freeing a handle from Java must not invalidate state still in use by an in-flight operation.

---

## 2. Document or enforce “no free while operation is pending”

### Problem

Even if the raw-pointer issue is fixed later, the current ABI still appears to rely on the caller not freeing a handle while an operation referencing it is pending.

### Why this matters

Java wrapper authors need a **hard contract**. If this rule exists, it must be stated explicitly. If possible, the ABI should remove the need for the rule entirely.

### Required fix

Choose one:

- **Preferred:** eliminate the rule by internal refcounting
- **Fallback:** formally document that handles must remain alive until all dependent operations complete

### Recommendation

Do not rely on documentation alone. Make the native layer safe by construction.

---

## 3. Make runtime initialization guarantees explicit

### Problem

Async work is spawned through Tokio, but the ABI does not make the runtime contract obvious.

### Risk

If Java calls into the library before the runtime exists, failures may be confusing and hard to diagnose.

### Required fix

Provide one of:

- `iroh_ffi_runtime_init()`
- lazy singleton runtime init inside the bridge
- explicit spec text stating runtime initialization happens automatically before any async call

### Recommendation

A lazy singleton runtime is simplest for Java consumers.

---

# Priority 1: make the ABI fit Java FFM better

## 4. Add nonblocking operation probing

### Problem

`iroh_ffi_operation_wait` is currently the main wait primitive.

### Why this is suboptimal for Java

Blocking inside a foreign call is awkward for Java, especially with virtual threads. Java can still wrap it asynchronously, but it is a poorer fit than a nonblocking probe.

### Required additions

Add functions like:

- `iroh_ffi_operation_is_ready(op, out_ready)`
- `iroh_ffi_operation_wait_timeout(op, timeout_ms)`
- optionally `iroh_ffi_operation_status(op, out_status)`

### Benefit

Java can build:

- `CompletableFuture`
- `Executor`-based pollers
- coroutine adapters
- reactive wrappers

without sitting in long blocking foreign calls.

---

## 5. Add operation cancellation

### Problem

Once an operation starts, the caller can only wait and then take the result.

### Why this matters

Java async abstractions need a cancellation story.

### Required addition

Add:

- `iroh_ffi_operation_cancel(op)`

### Notes

Cancellation can be best-effort or cooperative. It does not need to guarantee immediate stop, but Java must be able to request cancellation.

---

## 6. Add operation kind query

### Problem

The caller currently needs to know which `iroh_ffi_operation_take_*` matches a given operation.

### Why this matters

This makes the Java binding more brittle and increases the chance of mismatched extraction logic.

### Required addition

Add:

- `iroh_ffi_operation_kind(op, out_kind)`

### Benefit

Java can safely dispatch operation results and generate better diagnostics.

---

## 7. Consider a central completion queue later

### Problem

The current model is one condvar/result slot per operation.

### Assessment

This is acceptable for v1, but not ideal.

### Recommendation

Not required immediately, but worth considering later:

- submit operations
- poll a completion queue
- dispatch typed results/events centrally

### Benefit

A completion queue model is often a better long-term fit for Java async wrappers.

---

# Priority 2: complete endpoint configuration

## 8. Add full custom relay support

### Problem

The endpoint config currently supports relay modes like:

- default
- disabled
- staging

but not arbitrary caller-supplied relay URLs.

### Why this is a blocker

A stated requirement is that Java must be able to specify **custom relays**.

### Required change

Extend `iroh_ffi_endpoint_config_t` with something like:

```c
typedef struct {
    int32_t relay_mode;
    const iroh_ffi_bytes_t* alpns;
    size_t alpns_len;
    iroh_ffi_bytes_t secret_key;

    const iroh_ffi_string_t* relay_urls;
    size_t relay_urls_len;
} iroh_ffi_endpoint_config_t;
```

### Expected behavior

- if `relay_mode == CUSTOM`, use the supplied URLs
- otherwise use the selected built-in mode

---

## 9. Add secret-key export

### Problem

The bridge supports secret-key input, but not clearly secret-key export.

### Why this matters

If Java is managing strong node identity, it often needs to:

- create a new identity
- persist it
- restore it later

### Required addition

Expose one of:

- `iroh_ffi_net_export_secret_key`
- `iroh_ffi_node_export_secret_key`
- similar endpoint-level identity export

### Recommendation

Export as raw bytes with explicit free semantics.

---

## 10. Clarify ALPN ownership and semantics

### Problem

The config includes ALPNs, but the spec is not yet explicit enough about their meaning.

### Required clarifications

Document:

- whether ALPN values are arbitrary binary or expected UTF-8-like byte sequences
- whether connect must use one of the configured ALPNs
- whether endpoint config ALPNs register listeners or simply declare allowed protocols

### Why this matters

Java bindings need deterministic rules when translating user configuration.

---

# Priority 3: tighten memory ownership rules

## 11. Write explicit ownership rules for all returned values

### Problem

Free functions exist, but the contract is not yet explicit enough.

### Required documentation

For each returned type, specify:

- who allocates it
- who owns it
- how to free it
- whether it is safe to move/copy it
- whether it remains valid after dependent handles are freed

### At minimum document

- `iroh_ffi_string_t`
- `iroh_ffi_bytes_t`
- `iroh_ffi_node_addr_t`
- all handle pointers
- all operation handles

### Recommendation

Add a dedicated “Ownership Rules” section to the header/spec.

---

## 12. Define take-once behavior clearly

### Problem

Operation result extraction appears consumptive, but the ABI contract should state that unambiguously.

### Required rule

Define that:

- each operation result may be taken exactly once
- subsequent `take_*` calls fail with a defined error

### Nice-to-have

Add a stable error code such as:

- `IROH_FFI_ERR_ALREADY_TAKEN`

---

## 13. Clarify absent vs empty semantics

### Problem

Optional bytes and optional integers need precise semantics for Java.

### Required clarifications

Document whether:

- empty bytes (`len = 0`) are distinct from absent bytes
- absent integer values use a flag or a sentinel
- “none” values are stable across all APIs

### Recommendation

Prefer explicit presence flags over sentinel values.

---

## 14. Consider replacing `bool` fields in public structs

### Problem

Rust `bool` in `#[repr(C)]` structs often works, but is not the most conservative cross-language ABI choice.

### Recommendation

Consider replacing public `bool` fields with:

- `uint8_t present`
- `uint8_t success`
- `uint8_t has_value`

### Assessment

This is not a blocker for Java alone, but improves portability across more FFI consumers.

---

# Priority 4: improve the error model

## 15. Add stable numeric error codes

### Problem

`last_error_message` is useful, but strings alone are not enough for robust Java-side mapping.

### Required addition

Define stable numeric error codes for categories like:

- invalid argument
- timeout
- not found
- connection closed
- protocol error
- relay/discovery failure
- internal error
- unsupported operation

### Benefit

Java can map native failures to useful exception types without parsing text.

---

## 16. Document thread-local error retrieval rules

### Problem

If `LAST_ERROR` is thread-local, the caller must retrieve it from the same thread that observed the failure.

### Why this matters

Java wrappers may hop threads or use executor pools.

### Required documentation

State explicitly:

- `iroh_ffi_last_error_message()` must be called on the same native-calling thread that received the failing status

### Recommendation

This should be stated in both the spec and the header comments.

---

# Priority 5: API ergonomics and completeness

## 17. Add timeout-aware operation flows

### Problem

Some operations may take a long time:

- accept
- connect
- stream acceptance
- datagram reads
- long recv operations
- gossip receive/subscription flows

### Required improvement

At least one of:

- timeout-capable wait function
- cancelable operations
- timeout arguments on selected APIs

### Recommendation

`iroh_ffi_operation_wait_timeout` plus cancellation is enough for v1.

---

## 18. Clarify stream lifecycle semantics

### Problem

The stream API exposes useful operations, but the semantics need to be fully spelled out for Java wrapper authors.

### Document explicitly

- difference between `finish` and `close`
- when the peer observes EOF
- what `stopped` means
- how stop/reset codes propagate
- how send/recv halves interact

### Why this matters

Java async wrappers depend on deterministic stream state behavior.

---

## 19. Consider a lower-copy path later

### Problem

`write_all` and similar APIs likely copy into Rust-owned buffers.

### Assessment

Fine for v1.

### Future improvement

Later consider:

- direct-buffer friendly entry points
- caller-provided buffer slices
- chunked/vectored write interfaces

### Recommendation

Do not block v1 on this unless performance measurements demand it.

---

## 20. Keep service APIs consistent across blobs/docs/gossip/net

### Problem

As the surface grows, inconsistency will make Java bindings harder to generate and maintain.

### Required principle

All async service APIs should follow the same pattern:

1. submit operation
2. optionally poll or wait
3. take typed result
4. free resources explicitly

### Benefit

Java bindings stay predictable and easier to wrap.

---

# Function-level review themes

## Endpoint creation

### Current state

Good starting point.

### Needed improvements

- full custom relay support
- possible secret-key export symmetry
- clear ALPN semantics

---

## Node client getters

### Current state

Ergonomically good.

### Risk

These are only truly safe once in-flight handle lifetime is fixed.

---

## Connect / accept APIs

### Current state

Correct general shape for Java FFM.

### Risk

These are high-risk until raw-pointer resurrection is removed.

---

## Stream open/accept APIs

### Current state

Good ABI shape.

### Recommendation

Keep them, but document stream lifecycle semantics fully.

---

## Send/recv APIs

### Current state

Usable for Java.

### Notes

- current copy-heavy design is acceptable for v1
- absent-vs-empty semantics should be documented clearly

---

## Operation APIs

### Current state

Viable, but too blocking-oriented.

### Needed improvements

- readiness probe
- timeout wait
- cancellation
- operation kind query

---

## Error retrieval API

### Current state

Acceptable baseline.

### Needed improvement

Add stable numeric code taxonomy and document thread-local behavior clearly.

---

# Recommended implementation order

## Blockers first

1. Replace raw pointer resurrection with `Arc`-backed handle internals
2. Add full custom relay URL support

## Then Java-ergonomics improvements

3. Add `iroh_ffi_operation_is_ready`
4. Add `iroh_ffi_operation_wait_timeout`
5. Add `iroh_ffi_operation_cancel`
6. Add `iroh_ffi_operation_kind`

## Then spec hardening

7. Write explicit ownership rules
8. Write explicit thread-local error retrieval rules
9. Clarify ALPN and stream semantics
10. Add stable numeric error codes

## Optional later improvements

11. Completion queue redesign
12. Direct-buffer-friendly low-copy paths

---

# Final conclusion

The bridge is now **structurally valid as a Java FFI / FFM target**.

It is no longer “just a Python wrapper.” It has the right broad shape for:

- Java FFM
- Kotlin wrappers
- other language bindings through the same C ABI

However, I would still treat the following as **blocking issues** before binding it in earnest from Java:

1. **in-flight handle lifetime safety**
2. **full custom relay URL support**

After those are fixed, the bridge becomes realistic to wrap in:

- `CompletableFuture`
- `Flow.Publisher`
- Kotlin `suspend` / `Flow`

without redesigning the ABI.
