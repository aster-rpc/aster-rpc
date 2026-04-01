use std::cell::RefCell;
use std::ffi::{c_char, CString};
use std::ptr;
use std::sync::{Arc, Condvar, Mutex};

use iroh_transport_core::*;

pub const IROH_FFI_OK: i32 = 0;
pub const IROH_FFI_ERR_NULL: i32 = 1;
pub const IROH_FFI_ERR_INVALID_ARGUMENT: i32 = 2;
pub const IROH_FFI_ERR_INTERNAL: i32 = 3;
pub const IROH_FFI_ERR_PENDING: i32 = 4;
pub const IROH_FFI_ERR_FAILED: i32 = 5;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl ToString) -> i32 {
    let mut s = msg.to_string();
    if s.contains('\0') {
        s = s.replace('\0', " ");
    }
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = CString::new(s).ok();
    });
    IROH_FFI_ERR_FAILED
}

fn into_string(s: String) -> iroh_ffi_string_t {
    let mut bytes = s.into_bytes();
    let len = bytes.len();
    bytes.push(0);
    let ptr = bytes.as_mut_ptr() as *mut c_char;
    std::mem::forget(bytes);
    iroh_ffi_string_t { ptr, len }
}

fn into_bytes(bytes: Vec<u8>) -> iroh_ffi_bytes_t {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed) as *mut u8;
    iroh_ffi_bytes_t { ptr, len }
}

unsafe fn bytes_from_ref(b: &iroh_ffi_bytes_t) -> Vec<u8> {
    if b.ptr.is_null() || b.len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(b.ptr as *const u8, b.len).to_vec()
    }
}

unsafe fn string_from_ref(s: &iroh_ffi_string_t) -> Result<String, i32> {
    if s.ptr.is_null() {
        return Ok(String::new());
    }
    let slice = std::slice::from_raw_parts(s.ptr as *const u8, s.len);
    String::from_utf8(slice.to_vec()).map_err(set_last_error)
}

unsafe fn node_addr_from_ffi(addr: &iroh_ffi_node_addr_t) -> Result<CoreNodeAddr, i32> {
    let mut direct_addresses = Vec::new();
    if !addr.direct_addresses.is_null() {
        let items = std::slice::from_raw_parts(addr.direct_addresses, addr.direct_addresses_len);
        for item in items {
            direct_addresses.push(string_from_ref(item)?);
        }
    }
    let relay_url = string_from_ref(&addr.relay_url)?;
    Ok(CoreNodeAddr {
        endpoint_id: string_from_ref(&addr.endpoint_id)?,
        relay_url: if relay_url.is_empty() { None } else { Some(relay_url) },
        direct_addresses,
    })
}

unsafe fn endpoint_config_from_ffi(config: &iroh_ffi_endpoint_config_t) -> Result<CoreEndpointConfig, i32> {
    let relay_mode = match config.relay_mode {
        0 => None,
        1 => Some("default".to_string()),
        2 => Some("disabled".to_string()),
        3 => Some("staging".to_string()),
        _ => return Err(set_last_error("invalid relay_mode")),
    };
    let alpns = if config.alpns.is_null() || config.alpns_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(config.alpns, config.alpns_len)
            .iter()
            .map(|b| bytes_from_ref(b))
            .collect()
    };
    let secret_key = if config.secret_key.ptr.is_null() || config.secret_key.len == 0 {
        None
    } else {
        Some(bytes_from_ref(&config.secret_key))
    };
    Ok(CoreEndpointConfig {
        relay_mode,
        alpns,
        secret_key,
    })
}

fn node_addr_to_ffi(addr: CoreNodeAddr) -> iroh_ffi_node_addr_t {
    let mut directs: Vec<iroh_ffi_string_t> = addr
        .direct_addresses
        .into_iter()
        .map(into_string)
        .collect();
    let direct_addresses_len = directs.len();
    let direct_addresses = if directs.is_empty() {
        ptr::null_mut()
    } else {
        let p = directs.as_mut_ptr();
        std::mem::forget(directs);
        p
    };
    iroh_ffi_node_addr_t {
        endpoint_id: into_string(addr.endpoint_id),
        relay_url: into_string(addr.relay_url.unwrap_or_default()),
        direct_addresses,
        direct_addresses_len,
    }
}

fn closed_info_to_ffi(info: CoreClosedInfo) -> iroh_ffi_closed_info_t {
    iroh_ffi_closed_info_t {
        kind: into_string(info.kind),
        code_present: info.code.is_some(),
        code: info.code.unwrap_or_default(),
        reason: into_bytes(info.reason.unwrap_or_default()),
    }
}

fn gossip_event_to_ffi(event: CoreGossipEvent) -> iroh_ffi_gossip_event_t {
    iroh_ffi_gossip_event_t {
        event_type: into_string(event.event_type),
        data_present: event.data.is_some(),
        data: into_bytes(event.data.unwrap_or_default()),
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct iroh_ffi_bytes_t {
    pub ptr: *mut u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct iroh_ffi_string_t {
    pub ptr: *mut c_char,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct iroh_ffi_node_addr_t {
    pub endpoint_id: iroh_ffi_string_t,
    pub relay_url: iroh_ffi_string_t,
    pub direct_addresses: *mut iroh_ffi_string_t,
    pub direct_addresses_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct iroh_ffi_endpoint_config_t {
    pub relay_mode: i32,
    pub alpns: *const iroh_ffi_bytes_t,
    pub alpns_len: usize,
    pub secret_key: iroh_ffi_bytes_t,
}

#[repr(C)]
pub struct iroh_ffi_closed_info_t {
    pub kind: iroh_ffi_string_t,
    pub code_present: bool,
    pub code: u64,
    pub reason: iroh_ffi_bytes_t,
}

#[repr(C)]
pub struct iroh_ffi_gossip_event_t {
    pub event_type: iroh_ffi_string_t,
    pub data_present: bool,
    pub data: iroh_ffi_bytes_t,
}

pub struct NodeHandle(pub CoreNode);
pub struct NetHandle(pub CoreNetClient);
pub struct ConnectionHandle(pub CoreConnection);
pub struct SendStreamHandle(pub CoreSendStream);
pub struct RecvStreamHandle(pub CoreRecvStream);
pub struct BlobsHandle(pub CoreBlobsClient);
pub struct DocsHandle(pub CoreDocsClient);
pub struct DocHandle(pub CoreDoc);
pub struct GossipHandle(pub CoreGossipClient);
pub struct TopicHandle(pub CoreGossipTopic);

enum OpResult {
    Unit,
    String(String),
    Bytes(Vec<u8>),
    OptionalBytes(Option<Vec<u8>>),
    OptionalU64(Option<u64>),
    ClosedInfo(CoreClosedInfo),
    GossipEvent(CoreGossipEvent),
    Node(*mut NodeHandle),
    Net(*mut NetHandle),
    Connection(*mut ConnectionHandle),
    SendStream(*mut SendStreamHandle),
    RecvStream(*mut RecvStreamHandle),
    Doc(*mut DocHandle),
    Topic(*mut TopicHandle),
    BiStreams(*mut SendStreamHandle, *mut RecvStreamHandle),
}

unsafe impl Send for OpResult {}

struct OpState {
    result: Mutex<Option<Result<OpResult, String>>>,
    cv: Condvar,
}

pub struct OperationHandle {
    state: Arc<OpState>,
}

fn ptr_id<T>(ptr: *mut T) -> usize {
    ptr as usize
}

unsafe fn ptr_ref<'a, T>(id: usize) -> &'a T {
    &*(id as *const T)
}

fn spawn_op<F>(fut: F) -> *mut OperationHandle
where
    F: std::future::Future<Output = Result<OpResult, String>> + Send + 'static,
{
    let state = Arc::new(OpState {
        result: Mutex::new(None),
        cv: Condvar::new(),
    });
    let state2 = state.clone();
    tokio::spawn(async move {
        let result = fut.await;
        let mut slot = state2.result.lock().unwrap();
        *slot = Some(result);
        state2.cv.notify_all();
    });
    Box::into_raw(Box::new(OperationHandle { state }))
}

unsafe fn take_box<T>(ptr: *mut T) -> Box<T> {
    Box::from_raw(ptr)
}

fn take_result(op: *mut OperationHandle) -> Result<OpResult, i32> {
    if op.is_null() {
        return Err(IROH_FFI_ERR_NULL);
    }
    let op = unsafe { &*op };
    let mut slot = op.state.result.lock().unwrap();
    match slot.take() {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(set_last_error(e)),
        None => Err(IROH_FFI_ERR_PENDING),
    }
}

#[no_mangle]
pub extern "C" fn iroh_ffi_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ref().map(|s| s.as_ptr()).unwrap_or(ptr::null()))
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_bytes_free(bytes: iroh_ffi_bytes_t) {
    if !bytes.ptr.is_null() {
        let slice_ptr = std::ptr::slice_from_raw_parts_mut(bytes.ptr, bytes.len);
        drop(Box::from_raw(slice_ptr));
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_string_free(s: iroh_ffi_string_t) {
    if !s.ptr.is_null() {
        let _ = Vec::from_raw_parts(s.ptr as *mut u8, s.len + 1, s.len + 1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_addr_free(addr: iroh_ffi_node_addr_t) {
    iroh_ffi_string_free(addr.endpoint_id);
    iroh_ffi_string_free(addr.relay_url);
    if !addr.direct_addresses.is_null() {
        let items = Vec::from_raw_parts(addr.direct_addresses, addr.direct_addresses_len, addr.direct_addresses_len);
        for item in items {
            iroh_ffi_string_free(item);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_closed_info_free(info: iroh_ffi_closed_info_t) {
    iroh_ffi_string_free(info.kind);
    iroh_ffi_bytes_free(info.reason);
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_gossip_event_free(event: iroh_ffi_gossip_event_t) {
    iroh_ffi_string_free(event.event_type);
    iroh_ffi_bytes_free(event.data);
}

macro_rules! impl_free_handle {
    ($name:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(ptr: *mut $ty) {
            if !ptr.is_null() {
                drop(take_box(ptr));
            }
        }
    };
}

impl_free_handle!(iroh_ffi_node_free, NodeHandle);
impl_free_handle!(iroh_ffi_net_free, NetHandle);
impl_free_handle!(iroh_ffi_connection_free, ConnectionHandle);
impl_free_handle!(iroh_ffi_send_stream_free, SendStreamHandle);
impl_free_handle!(iroh_ffi_recv_stream_free, RecvStreamHandle);
impl_free_handle!(iroh_ffi_blobs_free, BlobsHandle);
impl_free_handle!(iroh_ffi_docs_free, DocsHandle);
impl_free_handle!(iroh_ffi_doc_free, DocHandle);
impl_free_handle!(iroh_ffi_gossip_free, GossipHandle);
impl_free_handle!(iroh_ffi_topic_free, TopicHandle);
impl_free_handle!(iroh_ffi_operation_free, OperationHandle);

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_wait(op: *mut OperationHandle) -> i32 {
    if op.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    let op = &*op;
    let mut slot = op.state.result.lock().unwrap();
    while slot.is_none() {
        slot = op.state.cv.wait(slot).unwrap();
    }
    IROH_FFI_OK
}

#[no_mangle]
pub extern "C" fn iroh_ffi_node_memory(out_op: *mut *mut OperationHandle) -> i32 {
    if out_op.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    unsafe {
        *out_op = spawn_op(async {
            CoreNode::memory()
                .await
                .map(|n| OpResult::Node(Box::into_raw(Box::new(NodeHandle(n)))))
                .map_err(|e| e.to_string())
        });
    }
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_persistent(path: iroh_ffi_string_t, out_op: *mut *mut OperationHandle) -> i32 {
    if out_op.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    let path = match string_from_ref(&path) {
        Ok(v) => v,
        Err(c) => return c,
    };
    *out_op = spawn_op(async move {
        CoreNode::persistent(path)
            .await
            .map(|n| OpResult::Node(Box::into_raw(Box::new(NodeHandle(n)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_endpoint_create(alpn: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if out_op.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    let alpn = bytes_from_ref(&alpn);
    *out_op = spawn_op(async move {
        CoreNetClient::create(alpn)
            .await
            .map(|n| OpResult::Net(Box::into_raw(Box::new(NetHandle(n)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_endpoint_create_with_config(config: *const iroh_ffi_endpoint_config_t, out_op: *mut *mut OperationHandle) -> i32 {
    if config.is_null() || out_op.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    let config = match endpoint_config_from_ffi(&*config) {
        Ok(v) => v,
        Err(c) => return c,
    };
    *out_op = spawn_op(async move {
        CoreNetClient::create_with_config(config)
            .await
            .map(|n| OpResult::Net(Box::into_raw(Box::new(NetHandle(n)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_id(node: *mut NodeHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if node.is_null() || out.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    *out = into_string((&*node).0.node_id());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_addr(node: *mut NodeHandle, out: *mut iroh_ffi_node_addr_t) -> i32 {
    if node.is_null() || out.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    *out = node_addr_to_ffi((&*node).0.node_addr_info());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_addr_debug(node: *mut NodeHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if node.is_null() || out.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    *out = into_string((&*node).0.node_addr_debug());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_add_node_addr(node: *mut NodeHandle, other: *mut NodeHandle) -> i32 {
    if node.is_null() || other.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    match (&*node).0.add_node_addr(&(&*other).0) {
        Ok(_) => IROH_FFI_OK,
        Err(e) => set_last_error(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_node_close(node: *mut NodeHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if node.is_null() || out_op.is_null() {
        return IROH_FFI_ERR_NULL;
    }
    let node = ptr_id(node);
    *out_op = spawn_op(async move {
        ptr_ref::<NodeHandle>(node).0.close().await;
        Ok(OpResult::Unit)
    });
    IROH_FFI_OK
}

macro_rules! sync_client_from_node {
    ($name:ident, $out_ty:ty, $ctor:expr) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(node: *mut NodeHandle, out: *mut *mut $out_ty) -> i32 {
            if node.is_null() || out.is_null() {
                return IROH_FFI_ERR_NULL;
            }
            *out = Box::into_raw(Box::new($ctor(&(&*node).0)));
            IROH_FFI_OK
        }
    };
}

sync_client_from_node!(iroh_ffi_node_blobs_client, BlobsHandle, |n: &CoreNode| BlobsHandle(n.blobs_client()));
sync_client_from_node!(iroh_ffi_node_docs_client, DocsHandle, |n: &CoreNode| DocsHandle(n.docs_client()));
sync_client_from_node!(iroh_ffi_node_gossip_client, GossipHandle, |n: &CoreNode| GossipHandle(n.gossip_client()));
sync_client_from_node!(iroh_ffi_node_net_client, NetHandle, |n: &CoreNode| NetHandle(n.net_client()));

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_endpoint_id(net: *mut NetHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if net.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    *out = into_string((&*net).0.endpoint_id());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_endpoint_addr(net: *mut NetHandle, out: *mut iroh_ffi_node_addr_t) -> i32 {
    if net.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    *out = node_addr_to_ffi((&*net).0.endpoint_addr_info());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_endpoint_addr_debug(net: *mut NetHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if net.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    *out = into_string((&*net).0.endpoint_addr_debug());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_close(net: *mut NetHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if net.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let net = ptr_id(net);
    *out_op = spawn_op(async move {
        ptr_ref::<NetHandle>(net).0.close().await;
        Ok(OpResult::Unit)
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_closed(net: *mut NetHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if net.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let net = ptr_id(net);
    *out_op = spawn_op(async move {
        ptr_ref::<NetHandle>(net).0.closed().await;
        Ok(OpResult::Unit)
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_connect(net: *mut NetHandle, node_id: iroh_ffi_string_t, alpn: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if net.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let node_id = match string_from_ref(&node_id) { Ok(v) => v, Err(c) => return c };
    let alpn = bytes_from_ref(&alpn);
    let net = ptr_id(net);
    *out_op = spawn_op(async move {
        ptr_ref::<NetHandle>(net).0.connect(node_id, alpn).await
            .map(|c| OpResult::Connection(Box::into_raw(Box::new(ConnectionHandle(c)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_connect_node_addr(net: *mut NetHandle, addr: *const iroh_ffi_node_addr_t, alpn: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if net.is_null() || addr.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let addr = match node_addr_from_ffi(&*addr) { Ok(v) => v, Err(c) => return c };
    let alpn = bytes_from_ref(&alpn);
    let net = ptr_id(net);
    *out_op = spawn_op(async move {
        ptr_ref::<NetHandle>(net).0.connect_node_addr(addr, alpn).await
            .map(|c| OpResult::Connection(Box::into_raw(Box::new(ConnectionHandle(c)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_net_accept(net: *mut NetHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if net.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let net = ptr_id(net);
    *out_op = spawn_op(async move {
        ptr_ref::<NetHandle>(net).0.accept().await
            .map(|c| OpResult::Connection(Box::into_raw(Box::new(ConnectionHandle(c)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_remote_id(conn: *mut ConnectionHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if conn.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    *out = into_string((&*conn).0.remote_id());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_open_bi(conn: *mut ConnectionHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if conn.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let conn = ptr_id(conn);
    *out_op = spawn_op(async move {
        ptr_ref::<ConnectionHandle>(conn).0.open_bi().await
            .map(|(s, r)| OpResult::BiStreams(Box::into_raw(Box::new(SendStreamHandle(s))), Box::into_raw(Box::new(RecvStreamHandle(r)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_accept_bi(conn: *mut ConnectionHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if conn.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let conn = ptr_id(conn);
    *out_op = spawn_op(async move {
        ptr_ref::<ConnectionHandle>(conn).0.accept_bi().await
            .map(|(s, r)| OpResult::BiStreams(Box::into_raw(Box::new(SendStreamHandle(s))), Box::into_raw(Box::new(RecvStreamHandle(r)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_open_uni(conn: *mut ConnectionHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if conn.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let conn = ptr_id(conn);
    *out_op = spawn_op(async move {
        ptr_ref::<ConnectionHandle>(conn).0.open_uni().await
            .map(|s| OpResult::SendStream(Box::into_raw(Box::new(SendStreamHandle(s)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_accept_uni(conn: *mut ConnectionHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if conn.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let conn = ptr_id(conn);
    *out_op = spawn_op(async move {
        ptr_ref::<ConnectionHandle>(conn).0.accept_uni().await
            .map(|r| OpResult::RecvStream(Box::into_raw(Box::new(RecvStreamHandle(r)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_send_datagram(conn: *mut ConnectionHandle, data: iroh_ffi_bytes_t) -> i32 {
    if conn.is_null() { return IROH_FFI_ERR_NULL; }
    match (&*conn).0.send_datagram(bytes_from_ref(&data)) {
        Ok(_) => IROH_FFI_OK,
        Err(e) => set_last_error(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_read_datagram(conn: *mut ConnectionHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if conn.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let conn = ptr_id(conn);
    *out_op = spawn_op(async move { ptr_ref::<ConnectionHandle>(conn).0.read_datagram().await.map(OpResult::Bytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_close(conn: *mut ConnectionHandle, code: u64, reason: iroh_ffi_bytes_t) -> i32 {
    if conn.is_null() { return IROH_FFI_ERR_NULL; }
    match (&*conn).0.close(code, bytes_from_ref(&reason)) {
        Ok(_) => IROH_FFI_OK,
        Err(e) => set_last_error(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_connection_closed(conn: *mut ConnectionHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if conn.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let conn = ptr_id(conn);
    *out_op = spawn_op(async move { Ok(OpResult::ClosedInfo(ptr_ref::<ConnectionHandle>(conn).0.closed().await)) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_send_stream_write_all(send: *mut SendStreamHandle, data: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if send.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let data = bytes_from_ref(&data);
    let send = ptr_id(send);
    *out_op = spawn_op(async move { ptr_ref::<SendStreamHandle>(send).0.write_all(data).await.map(|_| OpResult::Unit).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_send_stream_finish(send: *mut SendStreamHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if send.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let send = ptr_id(send);
    *out_op = spawn_op(async move { ptr_ref::<SendStreamHandle>(send).0.finish().await.map(|_| OpResult::Unit).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_send_stream_stopped(send: *mut SendStreamHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if send.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let send = ptr_id(send);
    *out_op = spawn_op(async move { ptr_ref::<SendStreamHandle>(send).0.stopped().await.map(OpResult::OptionalU64).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_recv_stream_read(recv: *mut RecvStreamHandle, max_len: usize, out_op: *mut *mut OperationHandle) -> i32 {
    if recv.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let recv = ptr_id(recv);
    *out_op = spawn_op(async move { ptr_ref::<RecvStreamHandle>(recv).0.read(max_len).await.map(OpResult::OptionalBytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_recv_stream_read_exact(recv: *mut RecvStreamHandle, n: usize, out_op: *mut *mut OperationHandle) -> i32 {
    if recv.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let recv = ptr_id(recv);
    *out_op = spawn_op(async move { ptr_ref::<RecvStreamHandle>(recv).0.read_exact(n).await.map(OpResult::Bytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_recv_stream_read_to_end(recv: *mut RecvStreamHandle, max_size: usize, out_op: *mut *mut OperationHandle) -> i32 {
    if recv.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let recv = ptr_id(recv);
    *out_op = spawn_op(async move { ptr_ref::<RecvStreamHandle>(recv).0.read_to_end(max_size).await.map(OpResult::Bytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_recv_stream_stop(recv: *mut RecvStreamHandle, code: u64) -> i32 {
    if recv.is_null() { return IROH_FFI_ERR_NULL; }
    match (&*recv).0.stop(code) {
        Ok(_) => IROH_FFI_OK,
        Err(e) => set_last_error(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_blobs_add_bytes(blobs: *mut BlobsHandle, data: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if blobs.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let data = bytes_from_ref(&data);
    let blobs = ptr_id(blobs);
    *out_op = spawn_op(async move { ptr_ref::<BlobsHandle>(blobs).0.add_bytes(data).await.map(OpResult::String).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_blobs_read_to_bytes(blobs: *mut BlobsHandle, hash: iroh_ffi_string_t, out_op: *mut *mut OperationHandle) -> i32 {
    if blobs.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let hash = match string_from_ref(&hash) { Ok(v) => v, Err(c) => return c };
    let blobs = ptr_id(blobs);
    *out_op = spawn_op(async move { ptr_ref::<BlobsHandle>(blobs).0.read_to_bytes(hash).await.map(OpResult::Bytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_blobs_create_ticket(blobs: *mut BlobsHandle, hash: iroh_ffi_string_t, out: *mut iroh_ffi_string_t) -> i32 {
    if blobs.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    let hash = match string_from_ref(&hash) { Ok(v) => v, Err(c) => return c };
    match (&*blobs).0.create_ticket(hash) {
        Ok(v) => { *out = into_string(v); IROH_FFI_OK }
        Err(e) => set_last_error(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_blobs_download_blob(blobs: *mut BlobsHandle, ticket: iroh_ffi_string_t, out_op: *mut *mut OperationHandle) -> i32 {
    if blobs.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let ticket = match string_from_ref(&ticket) { Ok(v) => v, Err(c) => return c };
    let blobs = ptr_id(blobs);
    *out_op = spawn_op(async move { ptr_ref::<BlobsHandle>(blobs).0.download_blob(ticket).await.map(OpResult::Bytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_docs_create(docs: *mut DocsHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if docs.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let docs = ptr_id(docs);
    *out_op = spawn_op(async move {
        ptr_ref::<DocsHandle>(docs).0.create().await
            .map(|d| OpResult::Doc(Box::into_raw(Box::new(DocHandle(d)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_docs_create_author(docs: *mut DocsHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if docs.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let docs = ptr_id(docs);
    *out_op = spawn_op(async move { ptr_ref::<DocsHandle>(docs).0.create_author().await.map(OpResult::String).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_docs_join(docs: *mut DocsHandle, ticket: iroh_ffi_string_t, out_op: *mut *mut OperationHandle) -> i32 {
    if docs.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let ticket = match string_from_ref(&ticket) { Ok(v) => v, Err(c) => return c };
    let docs = ptr_id(docs);
    *out_op = spawn_op(async move {
        ptr_ref::<DocsHandle>(docs).0.join(ticket).await
            .map(|d| OpResult::Doc(Box::into_raw(Box::new(DocHandle(d)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_doc_id(doc: *mut DocHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if doc.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    *out = into_string((&*doc).0.doc_id());
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_doc_set_bytes(doc: *mut DocHandle, author: iroh_ffi_string_t, key: iroh_ffi_bytes_t, value: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if doc.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let author = match string_from_ref(&author) { Ok(v) => v, Err(c) => return c };
    let key = bytes_from_ref(&key);
    let value = bytes_from_ref(&value);
    let doc = ptr_id(doc);
    *out_op = spawn_op(async move { ptr_ref::<DocHandle>(doc).0.set_bytes(author, key, value).await.map(OpResult::String).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_doc_get_exact(doc: *mut DocHandle, author: iroh_ffi_string_t, key: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if doc.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let author = match string_from_ref(&author) { Ok(v) => v, Err(c) => return c };
    let key = bytes_from_ref(&key);
    let doc = ptr_id(doc);
    *out_op = spawn_op(async move { ptr_ref::<DocHandle>(doc).0.get_exact(author, key).await.map(OpResult::OptionalBytes).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_doc_share(doc: *mut DocHandle, mode: iroh_ffi_string_t, out_op: *mut *mut OperationHandle) -> i32 {
    if doc.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let mode = match string_from_ref(&mode) { Ok(v) => v, Err(c) => return c };
    let doc = ptr_id(doc);
    *out_op = spawn_op(async move { ptr_ref::<DocHandle>(doc).0.share(mode).await.map(OpResult::String).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_gossip_subscribe(gossip: *mut GossipHandle, topic: iroh_ffi_bytes_t, peers: *const iroh_ffi_string_t, peers_len: usize, out_op: *mut *mut OperationHandle) -> i32 {
    if gossip.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let topic = bytes_from_ref(&topic);
    let peers = if peers.is_null() || peers_len == 0 {
        Vec::new()
    } else {
        let items = std::slice::from_raw_parts(peers, peers_len);
        let mut out = Vec::with_capacity(peers_len);
        for p in items {
            out.push(match string_from_ref(p) { Ok(v) => v, Err(c) => return c });
        }
        out
    };
    let gossip = ptr_id(gossip);
    *out_op = spawn_op(async move {
        ptr_ref::<GossipHandle>(gossip).0.subscribe(topic, peers).await
            .map(|t| OpResult::Topic(Box::into_raw(Box::new(TopicHandle(t)))))
            .map_err(|e| e.to_string())
    });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_topic_broadcast(topic: *mut TopicHandle, data: iroh_ffi_bytes_t, out_op: *mut *mut OperationHandle) -> i32 {
    if topic.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let data = bytes_from_ref(&data);
    let topic = ptr_id(topic);
    *out_op = spawn_op(async move { ptr_ref::<TopicHandle>(topic).0.broadcast(data).await.map(|_| OpResult::Unit).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_topic_recv(topic: *mut TopicHandle, out_op: *mut *mut OperationHandle) -> i32 {
    if topic.is_null() || out_op.is_null() { return IROH_FFI_ERR_NULL; }
    let topic = ptr_id(topic);
    *out_op = spawn_op(async move { ptr_ref::<TopicHandle>(topic).0.recv().await.map(OpResult::GossipEvent).map_err(|e| e.to_string()) });
    IROH_FFI_OK
}

macro_rules! take_handle_result {
    ($name:ident, $variant:ident, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(op: *mut OperationHandle, out: *mut *mut $ty) -> i32 {
            if out.is_null() { return IROH_FFI_ERR_NULL; }
            match take_result(op) {
                Ok(OpResult::$variant(v)) => { *out = v; IROH_FFI_OK }
                Ok(_) => set_last_error("unexpected operation result type"),
                Err(c) => c,
            }
        }
    };
}

take_handle_result!(iroh_ffi_operation_take_node, Node, NodeHandle);
take_handle_result!(iroh_ffi_operation_take_net, Net, NetHandle);
take_handle_result!(iroh_ffi_operation_take_connection, Connection, ConnectionHandle);
take_handle_result!(iroh_ffi_operation_take_send_stream, SendStream, SendStreamHandle);
take_handle_result!(iroh_ffi_operation_take_recv_stream, RecvStream, RecvStreamHandle);
take_handle_result!(iroh_ffi_operation_take_doc, Doc, DocHandle);
take_handle_result!(iroh_ffi_operation_take_topic, Topic, TopicHandle);

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_bi_streams(op: *mut OperationHandle, out_send: *mut *mut SendStreamHandle, out_recv: *mut *mut RecvStreamHandle) -> i32 {
    if out_send.is_null() || out_recv.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::BiStreams(s, r)) => { *out_send = s; *out_recv = r; IROH_FFI_OK }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_unit(op: *mut OperationHandle) -> i32 {
    match take_result(op) {
        Ok(OpResult::Unit) => IROH_FFI_OK,
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_string(op: *mut OperationHandle, out: *mut iroh_ffi_string_t) -> i32 {
    if out.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::String(v)) => { *out = into_string(v); IROH_FFI_OK }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_bytes(op: *mut OperationHandle, out: *mut iroh_ffi_bytes_t) -> i32 {
    if out.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::Bytes(v)) => { *out = into_bytes(v); IROH_FFI_OK }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_optional_bytes(op: *mut OperationHandle, out_present: *mut bool, out: *mut iroh_ffi_bytes_t) -> i32 {
    if out_present.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::OptionalBytes(v)) => {
            *out_present = v.is_some();
            *out = into_bytes(v.unwrap_or_default());
            IROH_FFI_OK
        }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_optional_u64(op: *mut OperationHandle, out_present: *mut bool, out: *mut u64) -> i32 {
    if out_present.is_null() || out.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::OptionalU64(v)) => {
            *out_present = v.is_some();
            *out = v.unwrap_or_default();
            IROH_FFI_OK
        }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_closed_info(op: *mut OperationHandle, out: *mut iroh_ffi_closed_info_t) -> i32 {
    if out.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::ClosedInfo(v)) => { *out = closed_info_to_ffi(v); IROH_FFI_OK }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}

#[no_mangle]
pub unsafe extern "C" fn iroh_ffi_operation_take_gossip_event(op: *mut OperationHandle, out: *mut iroh_ffi_gossip_event_t) -> i32 {
    if out.is_null() { return IROH_FFI_ERR_NULL; }
    match take_result(op) {
        Ok(OpResult::GossipEvent(v)) => { *out = gossip_event_to_ffi(v); IROH_FFI_OK }
        Ok(_) => set_last_error("unexpected operation result type"),
        Err(c) => c,
    }
}
