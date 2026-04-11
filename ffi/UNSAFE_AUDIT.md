# FFI Unsafe Audit (Task 5b.3)

## Scope
`ffi/src/lib.rs` — all `unsafe` blocks at the FFI boundary.

## Unsafe Patterns

### 1. `read_bytes` / `read_string` / `read_string_list`
**Pattern:** `slice::from_raw_parts(b.ptr, b.len)` on raw pointers from Java.

```rust
unsafe fn read_bytes(b: &iroh_bytes_t) -> Vec<u8> {
    if b.ptr.is_null() || b.len == 0 { Vec::new() }
    else { slice::from_raw_parts(b.ptr, b.len).to_vec() }
}
```

**Safety:** Assumes Java passes a valid, non-null pointer with correct length.
Null pointers are handled explicitly. Length 0 is handled.
**Miri concern:** Low. Miri tracks pointer validity. If Java passes an invalid
pointer, Miri will catch it. Validated by `iroh_buffer_release` buffer lifecycle.

---

### 2. `alloc_string` — **MEMORY LEAK BUG**
**Pattern:** `mem::forget(bytes)` without a corresponding deallocation function.

```rust
fn alloc_string(s: String) -> iroh_bytes_t {
    let mut bytes = s.into_bytes();
    let len = bytes.len();
    bytes.push(0); // null terminator
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes); // LEAK: no deallocation path
    iroh_bytes_t { ptr, len }
}
```

**Problem:** Java receives a raw pointer to owned memory that can never be freed.
There is no `iroh_string_release` equivalent. The memory is leaked for the
lifetime of the process.

**Impact:** Functions that return strings via `iroh_bytes_t` (e.g.,
`iroh_endpoint_remote_info_list`, `iroh_connection_info`) allocate via
`alloc_string` and leak each returned string.

**Fix required:** Either:
- Add a `iroh_string_release(runtime, ptr, len)` function that reconstructs
  the Box and drops it, or
- Switch to `alloc_bytes` (which uses `Box::into_raw` and IS tracked via the
  `buffers` map and `iroh_buffer_release`).

**Status:** LSan will NOT catch this because the process exits without freeing.
It would only be caught by long-running processes via RSS growth monitoring.

---

### 3. `alloc_bytes` — **SAFE** (with corresponding `iroh_buffer_release`)
**Pattern:** `Box::into_raw(bytes.into_boxed_slice())` — raw pointer stored in
the `buffers` map with a u64 key. Java receives the key and calls
`iroh_buffer_release(runtime, key)` to deallocate.

```rust
fn alloc_bytes(bytes: Vec<u8>) -> iroh_bytes_t {
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
    iroh_bytes_t { ptr, len }
}
```

**Safety:** Correct. Ownership is transferred to the `buffers` map and returned
to Java as a u64 key. `iroh_buffer_release` removes the entry and drops the Box.

---

### 4. Caller-provided buffer writes (`ptr::copy_nonoverlapping`)
**Pattern:** Write to `out_buf` (caller-allocated) using `ptr::copy_nonoverlapping`
or direct dereference. Bounds checks precede every write.

```rust
if offset + url.len() + 1 > buf_capacity {
    return iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32;
}
ptr::copy_nonoverlapping(url.as_ptr(), out_buf.add(offset), url.len());
```

**Safety:** Correct. Each write is preceded by a bounds check. Pointer offset
arithmetic via `.add()` is safe because capacity is validated before use.

---

### 5. Struct field pointer injection
**Pattern:** After writing string data into `out_buf`, construct a struct whose
`iroh_bytes_t` fields point into that buffer.

```rust
*out_addr = iroh_node_addr_t {
    endpoint_id: iroh_bytes_t {
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
`iroh_buffer_release` deallocation is also tracked. Should be Miri-clean.

**`alloc_string`:** Miri will NOT report a leak (process-exit leaks aren't
reported by Miri's leak sanitizer). However, ASan will flag the `mem::forget`
as a "leaked" allocation if it detects the memory isn't freed by process exit.

**Struct writes with `.add()`:** Miri validates pointer arithmetic. As long as
the bounds checks are passed, Miri considers these valid.

---

## CI Coverage

| Tool | Target | What it catches |
|------|--------|----------------|
| ASan | `aster_transport_ffi` lib + tests | Out-of-bounds, use-after-free, double-free |
| LSan | `aster_transport_ffi` lib + tests | Memory leaks (but not `alloc_string` process-lifetime leaks) |
| TSan | `aster_transport_ffi` lib + tests | Data races in concurrent FFI operations |
| Miri | `aster_transport_ffi` lib + cq_state_machine | UB in unsafe pointer operations |

---

## Recommended Fixes

### Fix `alloc_string` (HIGH PRIORITY — memory leak)

Add a deallocation function and use it for all string-returning FFI calls:

```rust
#[no_mangle]
pub unsafe extern "C" fn iroh_string_release(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 { return; }
    // Reconstruct the Box and drop it
    let boxed = Vec::from_raw_parts(ptr as *mut u8, len, len);
    drop(boxed);
}
```

Then update `alloc_string` to NOT use `mem::forget`. Instead, store the allocation
in the buffers map like `alloc_bytes` does. The Java side can then call
`iroh_buffer_release` with the returned key.

OR: Simply switch `alloc_string` to use `alloc_bytes` semantics:
store `Box::into_raw(bytes.into_boxed_slice())` in buffers map, return the key
alongside the `iroh_bytes_t` in the struct.

---

### Ensure Miri can run on FFI lib
Add Miri-specific test targets that only exercise the `lib` target and the
cq_state_machine integration test (which has no external dependencies).
