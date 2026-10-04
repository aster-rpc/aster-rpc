//! C ABI for the AEv1 RPC envelope codec and zstd payload compression.
//!
//! Thin wrappers over `aster_transport_core::envelope` (Aster-SPEC §6.9) and
//! `aster_transport_core::framing::{maybe_compress, decompress}` (§5.6), so
//! Java / Go / .NET share the one Rust implementation instead of each
//! re-implementing the envelope or linking their own zstd.
//!
//! # Ownership
//!
//! - **Encode** reads a caller-built struct (every `aster_bytes_t` /
//!   `aster_bytes_list_t` borrowed for the duration of the call; `storage`
//!   ignored) and returns the encoded bytes in a library-owned
//!   `aster_bytes_t`, released with `aster_string_release(ptr, len)`.
//! - **Decode** fills a caller-provided struct whose strings and lists point
//!   into one library-owned allocation referenced by `storage`. Release it
//!   exactly once with the matching `aster_*_release(&out)`, which frees the
//!   storage and zeroes the struct. Do not release a copy of the struct as
//!   well. Empty strings and empty lists are `{NULL, 0}`. Strings are UTF-8
//!   and *not* NUL-terminated.
//! - On a failed decode nothing is allocated and the out-struct is zeroed
//!   (except `struct_size`), so releasing it is a harmless no-op.
//!
//! # Errors
//!
//! `ASTER_STATUS_INVALID_ARGUMENT` for NULL pointers, a wrong `struct_size`,
//! non-UTF-8 strings, mismatched key/value list lengths, and malformed or
//! over-limit input; `ASTER_STATUS_INTERNAL` if zstd itself fails while
//! compressing. Every failure sets `aster_last_error_message`.

use std::ffi::c_void;
use std::slice;

use aster_transport_core::envelope::{self, CallHeader, RpcStatus, StreamHeader};
use aster_transport_core::framing;

use crate::{alloc_owned_bytes, aster_bytes_list_t, aster_bytes_t, aster_status_t, set_last_error};

// Literal values so cbindgen can emit them; pinned to core below.

/// Payloads strictly larger than this many bytes are compressed on send
/// (Aster-SPEC §5.6). Pass as `threshold` to `aster_zstd_maybe_compress`.
pub const ASTER_COMPRESSION_THRESHOLD: usize = 4096;
/// `aster_zstd_decompress` refuses output larger than this (16 MiB).
pub const ASTER_MAX_DECOMPRESSED_SIZE: usize = 16 * 1024 * 1024;
/// First byte of an encoded StreamHeader.
pub const ASTER_ENVELOPE_TAG_STREAM_HEADER: u8 = 0xA1;
/// First byte of an encoded CallHeader.
pub const ASTER_ENVELOPE_TAG_CALL_HEADER: u8 = 0xA2;
/// First byte of an encoded RpcStatus.
pub const ASTER_ENVELOPE_TAG_RPC_STATUS: u8 = 0xA3;
/// Envelope format version (second byte of every envelope).
pub const ASTER_ENVELOPE_VERSION: u8 = 0x01;

const _: () = assert!(ASTER_COMPRESSION_THRESHOLD == framing::COMPRESSION_THRESHOLD);
const _: () = assert!(ASTER_MAX_DECOMPRESSED_SIZE == framing::MAX_DECOMPRESSED_SIZE);
const _: () = assert!(ASTER_ENVELOPE_TAG_STREAM_HEADER == envelope::TAG_STREAM_HEADER);
const _: () = assert!(ASTER_ENVELOPE_TAG_CALL_HEADER == envelope::TAG_CALL_HEADER);
const _: () = assert!(ASTER_ENVELOPE_TAG_RPC_STATUS == envelope::TAG_RPC_STATUS);
const _: () = assert!(ASTER_ENVELOPE_VERSION == envelope::ENVELOPE_VERSION);

/// `StreamHeader` (payload of a `FLAG_HEADER` frame), Aster-SPEC §6.9.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_stream_header_t {
    /// `sizeof(aster_stream_header_t)`: set by the caller for encode, written
    /// by the library on decode.
    pub struct_size: u32,
    pub version: i32,
    pub call_id: i32,
    /// `0` = stateless SHARED-pool stream; non-zero = session-bound.
    pub session_id: i32,
    /// Relative seconds; `0` = no deadline.
    pub deadline: i16,
    /// XLANG=0, NATIVE=1, ROW=2 (JSON=3 is retired).
    pub serialization_mode: i8,
    /// Set to 0.
    pub reserved0: u8,
    /// Set to 0.
    pub reserved1: u32,
    pub service: aster_bytes_t,
    pub method: aster_bytes_t,
    pub metadata_keys: aster_bytes_list_t,
    /// Same length as `metadata_keys`.
    pub metadata_values: aster_bytes_list_t,
    /// Library-owned backing store of a decoded struct; NULL otherwise.
    pub storage: *mut c_void,
}

/// `CallHeader` (payload of a `FLAG_CALL` frame), Aster-SPEC §6.9.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_call_header_t {
    /// `sizeof(aster_call_header_t)`: set by the caller for encode, written
    /// by the library on decode.
    pub struct_size: u32,
    pub call_id: i32,
    /// Relative seconds; `0` = no deadline.
    pub deadline: i16,
    /// Set to 0.
    pub reserved0: u16,
    /// Set to 0.
    pub reserved1: u32,
    pub method: aster_bytes_t,
    pub metadata_keys: aster_bytes_list_t,
    /// Same length as `metadata_keys`.
    pub metadata_values: aster_bytes_list_t,
    /// Library-owned backing store of a decoded struct; NULL otherwise.
    pub storage: *mut c_void,
}

/// `RpcStatus` (payload of a `FLAG_TRAILER` frame), Aster-SPEC §6.9.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct aster_rpc_status_t {
    /// `sizeof(aster_rpc_status_t)`: set by the caller for encode, written by
    /// the library on decode.
    pub struct_size: u32,
    /// Status code (Aster-SPEC §6.5): 0 = OK, 1..16 gRPC, 100+ Aster.
    pub code: i32,
    pub message: aster_bytes_t,
    pub detail_keys: aster_bytes_list_t,
    /// Same length as `detail_keys`.
    pub detail_values: aster_bytes_list_t,
    /// Library-owned backing store of a decoded struct; NULL otherwise.
    pub storage: *mut c_void,
}

pub(crate) const EMPTY_BYTES: aster_bytes_t = aster_bytes_t {
    ptr: std::ptr::null(),
    len: 0,
};
const EMPTY_LIST: aster_bytes_list_t = aster_bytes_list_t {
    items: std::ptr::null(),
    len: 0,
};

pub(crate) fn invalid(msg: impl ToString) -> i32 {
    set_last_error(msg);
    aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
}

pub(crate) const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;

// ── input helpers ───────────────────────────────────────────────────────────

/// Borrow a caller buffer. `{NULL, 0}` is empty; `{NULL, n>0}` is invalid.
pub(crate) unsafe fn in_slice<'a>(ptr: *const u8, len: usize, what: &str) -> Result<&'a [u8], i32> {
    if len == 0 {
        Ok(&[])
    } else if ptr.is_null() {
        Err(invalid(format!("{what}: NULL pointer with length {len}")))
    } else {
        Ok(slice::from_raw_parts(ptr, len))
    }
}

pub(crate) unsafe fn in_string(b: &aster_bytes_t, what: &str) -> Result<String, i32> {
    let bytes = in_slice(b.ptr, b.len, what)?;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|e| invalid(format!("{what}: invalid UTF-8: {e}")))
}

unsafe fn in_list(l: &aster_bytes_list_t, what: &str) -> Result<Vec<String>, i32> {
    if l.len == 0 {
        return Ok(Vec::new());
    }
    if l.items.is_null() {
        return Err(invalid(format!("{what}: NULL items with length {}", l.len)));
    }
    slice::from_raw_parts(l.items, l.len)
        .iter()
        .enumerate()
        .map(|(i, b)| in_string(b, &format!("{what}[{i}]")))
        .collect()
}

pub(crate) fn check_struct_size<T>(got: u32, name: &str) -> Result<(), i32> {
    let want = std::mem::size_of::<T>();
    if got as usize != want {
        return Err(invalid(format!(
            "{name}.struct_size is {got}, expected {want}"
        )));
    }
    Ok(())
}

unsafe fn emit_encoded(r: envelope::Result<Vec<u8>>, out: *mut aster_bytes_t) -> i32 {
    match r {
        Ok(bytes) => {
            *out = alloc_owned_bytes(bytes);
            OK
        }
        Err(e) => invalid(e),
    }
}

// ── decoded storage ─────────────────────────────────────────────────────────

/// One allocation owning every string a decoded struct points at, plus the
/// `aster_bytes_t` arrays its lists point at. String heap buffers do not move
/// when the `String` itself is moved into the box, so pointers taken before
/// boxing stay valid until the box is dropped.
struct Storage {
    _strings: Vec<String>,
    lists: Vec<Vec<aster_bytes_t>>,
}

fn view(s: &str) -> aster_bytes_t {
    if s.is_empty() {
        EMPTY_BYTES
    } else {
        aster_bytes_t {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }
}

fn view_list(items: &[aster_bytes_t]) -> aster_bytes_list_t {
    if items.is_empty() {
        EMPTY_LIST
    } else {
        aster_bytes_list_t {
            items: items.as_ptr(),
            len: items.len(),
        }
    }
}

impl Storage {
    /// Box the strings; returns the box plus a view of each scalar string
    /// (`scalars` first entries of `strings`) and each list.
    fn build(
        scalars: Vec<String>,
        lists: Vec<Vec<String>>,
    ) -> (Box<Storage>, Vec<aster_bytes_t>, Vec<aster_bytes_list_t>) {
        let scalar_views: Vec<aster_bytes_t> = scalars.iter().map(|s| view(s)).collect();
        let list_items: Vec<Vec<aster_bytes_t>> = lists
            .iter()
            .map(|l| l.iter().map(|s| view(s)).collect())
            .collect();
        let mut strings = scalars;
        strings.extend(lists.into_iter().flatten());
        let storage = Box::new(Storage {
            _strings: strings,
            lists: list_items,
        });
        let list_views = storage.lists.iter().map(|l| view_list(l)).collect();
        (storage, scalar_views, list_views)
    }
}

unsafe fn release_storage(storage: &mut *mut c_void) {
    if !storage.is_null() {
        drop(Box::from_raw(*storage as *mut Storage));
        *storage = std::ptr::null_mut();
    }
}

// ── StreamHeader ────────────────────────────────────────────────────────────

/// Encode a StreamHeader to AEv1 bytes.
///
/// `header->struct_size` must be `sizeof(aster_stream_header_t)`;
/// `header->storage` is ignored. On success `*out` holds the encoding;
/// release it with `aster_string_release(out->ptr, out->len)`.
///
/// # Safety
/// `header` and `out` must be valid pointers; every buffer the struct
/// references must be readable for its stated length.
#[no_mangle]
pub unsafe extern "C" fn aster_stream_header_encode(
    header: *const aster_stream_header_t,
    out: *mut aster_bytes_t,
) -> i32 {
    if header.is_null() || out.is_null() {
        return invalid("aster_stream_header_encode: NULL argument");
    }
    *out = EMPTY_BYTES;
    let h = &*header;
    let build = || -> Result<StreamHeader, i32> {
        check_struct_size::<aster_stream_header_t>(h.struct_size, "aster_stream_header_t")?;
        Ok(StreamHeader {
            service: in_string(&h.service, "service")?,
            method: in_string(&h.method, "method")?,
            version: h.version,
            call_id: h.call_id,
            deadline: h.deadline,
            serialization_mode: h.serialization_mode,
            metadata_keys: in_list(&h.metadata_keys, "metadata_keys")?,
            metadata_values: in_list(&h.metadata_values, "metadata_values")?,
            session_id: h.session_id,
        })
    };
    match build() {
        Ok(v) => emit_encoded(envelope::encode_stream_header(&v), out),
        Err(s) => s,
    }
}

/// Decode AEv1 bytes into a StreamHeader.
///
/// On success `*out` is filled (including `struct_size`) and must be released
/// with `aster_stream_header_release(out)`. On failure `*out` is zeroed.
///
/// # Safety
/// `buf` must be readable for `len` bytes (may be NULL when `len == 0`);
/// `out` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn aster_stream_header_decode(
    buf: *const u8,
    len: usize,
    out: *mut aster_stream_header_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_stream_header_decode: NULL out");
    }
    let o = &mut *out;
    *o = aster_stream_header_t {
        struct_size: std::mem::size_of::<aster_stream_header_t>() as u32,
        version: 0,
        call_id: 0,
        session_id: 0,
        deadline: 0,
        serialization_mode: 0,
        reserved0: 0,
        reserved1: 0,
        service: EMPTY_BYTES,
        method: EMPTY_BYTES,
        metadata_keys: EMPTY_LIST,
        metadata_values: EMPTY_LIST,
        storage: std::ptr::null_mut(),
    };
    let input = match in_slice(buf, len, "buf") {
        Ok(b) => b,
        Err(s) => return s,
    };
    let h = match envelope::decode_stream_header(input) {
        Ok(h) => h,
        Err(e) => return invalid(e),
    };
    let (storage, s, l) = Storage::build(
        vec![h.service, h.method],
        vec![h.metadata_keys, h.metadata_values],
    );
    o.version = h.version;
    o.call_id = h.call_id;
    o.session_id = h.session_id;
    o.deadline = h.deadline;
    o.serialization_mode = h.serialization_mode;
    o.service = s[0];
    o.method = s[1];
    o.metadata_keys = l[0];
    o.metadata_values = l[1];
    o.storage = Box::into_raw(storage) as *mut c_void;
    OK
}

/// Free the storage of a StreamHeader filled by `aster_stream_header_decode`
/// and zero its fields (`struct_size` is kept). A NULL `header` or a struct
/// whose `storage` is NULL is a no-op.
///
/// # Safety
/// `header` must be NULL or point to a struct last written by
/// `aster_stream_header_decode` (or by a caller, with `storage == NULL`).
#[no_mangle]
pub unsafe extern "C" fn aster_stream_header_release(header: *mut aster_stream_header_t) -> i32 {
    if header.is_null() {
        return OK;
    }
    let h = &mut *header;
    release_storage(&mut h.storage);
    h.service = EMPTY_BYTES;
    h.method = EMPTY_BYTES;
    h.metadata_keys = EMPTY_LIST;
    h.metadata_values = EMPTY_LIST;
    OK
}

// ── CallHeader ──────────────────────────────────────────────────────────────

/// Encode a CallHeader to AEv1 bytes. Same contract as
/// `aster_stream_header_encode`.
///
/// # Safety
/// See `aster_stream_header_encode`.
#[no_mangle]
pub unsafe extern "C" fn aster_call_header_encode(
    header: *const aster_call_header_t,
    out: *mut aster_bytes_t,
) -> i32 {
    if header.is_null() || out.is_null() {
        return invalid("aster_call_header_encode: NULL argument");
    }
    *out = EMPTY_BYTES;
    let h = &*header;
    let build = || -> Result<CallHeader, i32> {
        check_struct_size::<aster_call_header_t>(h.struct_size, "aster_call_header_t")?;
        Ok(CallHeader {
            method: in_string(&h.method, "method")?,
            call_id: h.call_id,
            deadline: h.deadline,
            metadata_keys: in_list(&h.metadata_keys, "metadata_keys")?,
            metadata_values: in_list(&h.metadata_values, "metadata_values")?,
        })
    };
    match build() {
        Ok(v) => emit_encoded(envelope::encode_call_header(&v), out),
        Err(s) => s,
    }
}

/// Decode AEv1 bytes into a CallHeader. Same contract as
/// `aster_stream_header_decode`; release with `aster_call_header_release`.
///
/// # Safety
/// See `aster_stream_header_decode`.
#[no_mangle]
pub unsafe extern "C" fn aster_call_header_decode(
    buf: *const u8,
    len: usize,
    out: *mut aster_call_header_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_call_header_decode: NULL out");
    }
    let o = &mut *out;
    *o = aster_call_header_t {
        struct_size: std::mem::size_of::<aster_call_header_t>() as u32,
        call_id: 0,
        deadline: 0,
        reserved0: 0,
        reserved1: 0,
        method: EMPTY_BYTES,
        metadata_keys: EMPTY_LIST,
        metadata_values: EMPTY_LIST,
        storage: std::ptr::null_mut(),
    };
    let input = match in_slice(buf, len, "buf") {
        Ok(b) => b,
        Err(s) => return s,
    };
    let h = match envelope::decode_call_header(input) {
        Ok(h) => h,
        Err(e) => return invalid(e),
    };
    let (storage, s, l) = Storage::build(vec![h.method], vec![h.metadata_keys, h.metadata_values]);
    o.call_id = h.call_id;
    o.deadline = h.deadline;
    o.method = s[0];
    o.metadata_keys = l[0];
    o.metadata_values = l[1];
    o.storage = Box::into_raw(storage) as *mut c_void;
    OK
}

/// Free a CallHeader filled by `aster_call_header_decode`. See
/// `aster_stream_header_release`.
///
/// # Safety
/// See `aster_stream_header_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_call_header_release(header: *mut aster_call_header_t) -> i32 {
    if header.is_null() {
        return OK;
    }
    let h = &mut *header;
    release_storage(&mut h.storage);
    h.method = EMPTY_BYTES;
    h.metadata_keys = EMPTY_LIST;
    h.metadata_values = EMPTY_LIST;
    OK
}

// ── RpcStatus ───────────────────────────────────────────────────────────────

/// Encode an RpcStatus to AEv1 bytes. Same contract as
/// `aster_stream_header_encode`.
///
/// # Safety
/// See `aster_stream_header_encode`.
#[no_mangle]
pub unsafe extern "C" fn aster_rpc_status_encode(
    status: *const aster_rpc_status_t,
    out: *mut aster_bytes_t,
) -> i32 {
    if status.is_null() || out.is_null() {
        return invalid("aster_rpc_status_encode: NULL argument");
    }
    *out = EMPTY_BYTES;
    let s = &*status;
    let build = || -> Result<RpcStatus, i32> {
        check_struct_size::<aster_rpc_status_t>(s.struct_size, "aster_rpc_status_t")?;
        Ok(RpcStatus {
            code: s.code,
            message: in_string(&s.message, "message")?,
            detail_keys: in_list(&s.detail_keys, "detail_keys")?,
            detail_values: in_list(&s.detail_values, "detail_values")?,
        })
    };
    match build() {
        Ok(v) => emit_encoded(envelope::encode_rpc_status(&v), out),
        Err(st) => st,
    }
}

/// Decode AEv1 bytes into an RpcStatus. Same contract as
/// `aster_stream_header_decode`; release with `aster_rpc_status_release`.
///
/// # Safety
/// See `aster_stream_header_decode`.
#[no_mangle]
pub unsafe extern "C" fn aster_rpc_status_decode(
    buf: *const u8,
    len: usize,
    out: *mut aster_rpc_status_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_rpc_status_decode: NULL out");
    }
    let o = &mut *out;
    *o = aster_rpc_status_t {
        struct_size: std::mem::size_of::<aster_rpc_status_t>() as u32,
        code: 0,
        message: EMPTY_BYTES,
        detail_keys: EMPTY_LIST,
        detail_values: EMPTY_LIST,
        storage: std::ptr::null_mut(),
    };
    let input = match in_slice(buf, len, "buf") {
        Ok(b) => b,
        Err(s) => return s,
    };
    let st = match envelope::decode_rpc_status(input) {
        Ok(s) => s,
        Err(e) => return invalid(e),
    };
    let (storage, s, l) = Storage::build(vec![st.message], vec![st.detail_keys, st.detail_values]);
    o.code = st.code;
    o.message = s[0];
    o.detail_keys = l[0];
    o.detail_values = l[1];
    o.storage = Box::into_raw(storage) as *mut c_void;
    OK
}

/// Free an RpcStatus filled by `aster_rpc_status_decode`. See
/// `aster_stream_header_release`.
///
/// # Safety
/// See `aster_stream_header_release`.
#[no_mangle]
pub unsafe extern "C" fn aster_rpc_status_release(status: *mut aster_rpc_status_t) -> i32 {
    if status.is_null() {
        return OK;
    }
    let s = &mut *status;
    release_storage(&mut s.storage);
    s.message = EMPTY_BYTES;
    s.detail_keys = EMPTY_LIST;
    s.detail_values = EMPTY_LIST;
    OK
}

// ── zstd payload compression (Aster-SPEC §5.6) ──────────────────────────────

/// Compress a payload for sending if it is larger than `threshold` bytes
/// (normally `ASTER_COMPRESSION_THRESHOLD`), with zstd level 3.
///
/// On `ASTER_STATUS_OK`:
/// - `out->ptr != NULL`: send `out` with `FLAG_COMPRESSED` set; release it
///   with `aster_string_release(out->ptr, out->len)`.
/// - `*out == {NULL, 0}`: send the original payload unchanged (it was at or
///   below the threshold, or compression did not make it smaller).
///
/// # Safety
/// `data` must be readable for `len` bytes (may be NULL when `len == 0`);
/// `out` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn aster_zstd_maybe_compress(
    data: *const u8,
    len: usize,
    threshold: usize,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_zstd_maybe_compress: NULL out");
    }
    *out = EMPTY_BYTES;
    let input = match in_slice(data, len, "data") {
        Ok(b) => b,
        Err(s) => return s,
    };
    match framing::maybe_compress(input, threshold) {
        Ok(Some(c)) => {
            *out = alloc_owned_bytes(c);
            OK
        }
        Ok(None) => OK,
        Err(e) => set_last_error(e),
    }
}

/// Decompress a `FLAG_COMPRESSED` payload. Output larger than
/// `ASTER_MAX_DECOMPRESSED_SIZE` (16 MiB) and corrupt input are refused with
/// `ASTER_STATUS_INVALID_ARGUMENT`. On success release `out` with
/// `aster_string_release(out->ptr, out->len)` (an empty result is `{NULL, 0}`).
///
/// # Safety
/// `data` must be readable for `len` bytes (may be NULL when `len == 0`);
/// `out` must be a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn aster_zstd_decompress(
    data: *const u8,
    len: usize,
    out: *mut aster_bytes_t,
) -> i32 {
    if out.is_null() {
        return invalid("aster_zstd_decompress: NULL out");
    }
    *out = EMPTY_BYTES;
    let input = match in_slice(data, len, "data") {
        Ok(b) => b,
        Err(s) => return s,
    };
    match framing::decompress(input) {
        Ok(v) => {
            *out = alloc_owned_bytes(v);
            OK
        }
        Err(e) => invalid(e),
    }
}
