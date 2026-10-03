# FFI Unsafe Audit (Task 5b.3)

## Scope
`ffi/src/lib.rs` — all `unsafe` blocks at the FFI boundary.

## Unsafe Patterns

### 1. `read_bytes` / `read_string` / `read_string_list`
**Pattern:** `slice::from_raw_parts(b.ptr, b.len)` on raw pointers from Java.

```rust
unsafe fn read_bytes(b: &aster_bytes_t) -> Vec<u8> {
    if b.ptr.is_null() || b.len == 0 { Vec::new() }
    else { slice::from_raw_parts(b.ptr, b.len).to_vec() }
}
```

**Safety:** Assumes Java passes a valid, non-null pointer with correct length.
Null pointers are handled explicitly. Length 0 is handled.
**Miri concern:** Low. Miri tracks pointer validity. If Java passes an invalid
pointer, Miri will catch it. Validated by `aster_buffer_release` buffer lifecycle.

---

### 2. `alloc_string` / `alloc_bytes` + `aster_string_release` — **FIXED**
**Was:** `alloc_string` pushed a NUL onto a `Vec` and `mem::forget`-ed it
(capacity could be anything >= `len + 1`), while `aster_string_release` freed
it as `Vec::from_raw_parts(ptr, len, len)` — a wrong-size deallocation (UB).
Empty strings still allocated (for the NUL) but `len == 0` made release a
no-op, so they leaked.

**Now:** both helpers go through `alloc_owned_bytes`, which returns `{NULL, 0}`
for empty input (no allocation) and otherwise allocates exactly `len + 1`
bytes as a `Box<[u8]>` (trailing NUL, not counted in `len`).
`aster_string_release(ptr, len)` rebuilds the identical `Box<[u8]>` from
`(ptr, len + 1)`. The caller contract is unchanged (pass back the returned
`ptr`/`len`; NULL / 0 is a no-op). Covered by `string_release_tests` in
`ffi/src/lib.rs` (run under Miri to check the layouts).

---

### 3. `alloc_bytes` — **SAFE** (with corresponding `aster_buffer_release`)
**Pattern:** `Box::into_raw(bytes.into_boxed_slice())` — raw pointer stored in
the `buffers` map with a u64 key. Java receives the key and calls
`aster_buffer_release(runtime, key)` to deallocate.

```rust
fn alloc_bytes(bytes: Vec<u8>) -> aster_bytes_t {
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
    aster_bytes_t { ptr, len }
}
```

**Safety:** Correct. Ownership is transferred to the `buffers` map and returned
to Java as a u64 key. `aster_buffer_release` removes the entry and drops the Box.

---

### 4. Caller-provided buffer writes (`ptr::copy_nonoverlapping`)
**Pattern:** Write to `out_buf` (caller-allocated) using `ptr::copy_nonoverlapping`
or direct dereference. Bounds checks precede every write.

```rust
if offset + url.len() + 1 > buf_capacity {
    return aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32;
}
ptr::copy_nonoverlapping(url.as_ptr(), out_buf.add(offset), url.len());
```

**Safety:** Correct. Each write is preceded by a bounds check. Pointer offset
arithmetic via `.add()` is safe because capacity is validated before use.

---

### 5. Struct field pointer injection
**Pattern:** After writing string data into `out_buf`, construct a struct whose
`aster_bytes_t` fields point into that buffer.

```rust
*out_addr = aster_node_addr_t {
    endpoint_id: aster_bytes_t {
        ptr: unsafe { out_buf.add(ep_id_offset) }, // points into caller's buffer
        len: addr.endpoint_id.len(),
    },
    ...
};
```

**Safety:** Sound if caller keeps `out_buf` alive for the duration of the call.
The struct is passed by value (copied) so the pointers remain valid as long as
the original buffer is alive.

---

## Miri Compatibility

**`read_bytes`-family:** Miri validates pointer/length pairs. Any
out-of-bounds or invalid pointer passed from Java will be caught by Miri when
running the FFI tests under `cargo miri test`.

**`alloc_bytes`:** Miri tracks allocations via `Box::into_raw`. The
`aster_buffer_release` deallocation is also tracked. Should be Miri-clean.

**`alloc_string` / `aster_string_release`:** allocation and deallocation use
the same `Box<[u8]>` layout; `string_release_tests` exercise the round trip.

**Struct writes with `.add()`:** Miri validates pointer arithmetic. As long as
the bounds checks are passed, Miri considers these valid.

---

## CI Coverage

| Tool | Target | What it catches |
|------|--------|----------------|
| ASan | `aster_transport_ffi` lib + tests | Out-of-bounds, use-after-free, double-free |
| LSan | `aster_transport_ffi` lib + tests | Memory leaks |
| TSan | `aster_transport_ffi` lib + tests | Data races in concurrent FFI operations |
| Miri | `aster_transport_ffi` lib + cq_state_machine | UB in unsafe pointer operations |

---

## Recommended Fixes

### Ensure Miri can run on FFI lib
Add Miri-specific test targets that only exercise the `lib` target and the
cq_state_machine integration test (which has no external dependencies).
