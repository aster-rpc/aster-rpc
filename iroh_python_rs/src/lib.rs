use std::ptr;

use iroh_transport_ffi::*;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_asyncio::tokio::future_into_py;

create_exception!(_iroh_python, IrohError, PyException);

fn ffi_err() -> PyErr {
    unsafe {
        let ptr = iroh_ffi_last_error_message();
        if ptr.is_null() {
            IrohError::new_err("ffi error")
        } else {
            IrohError::new_err(std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

fn check(code: i32) -> PyResult<()> {
    if code == IROH_FFI_OK { Ok(()) } else { Err(ffi_err()) }
}

fn str_to_ffi(s: String) -> iroh_ffi_string_t {
    let mut raw = s.into_bytes();
    let len = raw.len();
    raw.push(0);
    let ptr = raw.as_mut_ptr() as *mut std::ffi::c_char;
    std::mem::forget(raw);
    iroh_ffi_string_t { ptr, len }
}

fn bytes_to_ffi(v: Vec<u8>) -> iroh_ffi_bytes_t {
    let boxed = v.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed) as *mut u8;
    iroh_ffi_bytes_t { ptr, len }
}

fn ptr_to_usize<T>(ptr: *mut T) -> usize { ptr as usize }

fn usize_to_ptr<T>(value: usize) -> *mut T { value as *mut T }

fn op_slot() -> usize { 0 }

fn op_ptr(slot: usize) -> *mut OperationHandle { usize_to_ptr(slot) }

fn set_op(slot: &mut usize, ptr: *mut OperationHandle) { *slot = ptr_to_usize(ptr); }

fn start_gossip_subscribe_op(inner: usize, topic_bytes: Vec<u8>, bootstrap_peers: Vec<String>) -> PyResult<usize> {
    let topic = bytes_to_ffi(topic_bytes);
    let peers: Vec<iroh_ffi_string_t> = bootstrap_peers.into_iter().map(str_to_ffi).collect();
    let mut op = op_slot();
    unsafe {
        let mut raw = ptr::null_mut();
        check(iroh_ffi_gossip_subscribe(
            usize_to_ptr(inner),
            topic,
            if peers.is_empty() { ptr::null() } else { peers.as_ptr() },
            peers.len(),
            &mut raw,
        ))?;
        iroh_ffi_bytes_free(topic);
        for p in peers { iroh_ffi_string_free(p); }
        set_op(&mut op, raw);
    }
    Ok(op)
}

fn start_net_connect_node_addr_op(inner: usize, addr: NodeAddr, alpn: Vec<u8>) -> PyResult<usize> {
    let addr = unsafe { addr.to_ffi() };
    let alpn = bytes_to_ffi(alpn);
    let mut op = op_slot();
    unsafe {
        let mut raw = ptr::null_mut();
        check(iroh_ffi_net_connect_node_addr(usize_to_ptr(inner), &addr, alpn, &mut raw))?;
        iroh_ffi_node_addr_free(addr);
        iroh_ffi_bytes_free(alpn);
        set_op(&mut op, raw);
    }
    Ok(op)
}

unsafe fn free_string_to_rust(s: iroh_ffi_string_t) -> String {
    let bytes = if s.ptr.is_null() || s.len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(s.ptr as *const u8, s.len).to_vec()
    };
    iroh_ffi_string_free(s);
    String::from_utf8(bytes).unwrap_or_default()
}

unsafe fn free_bytes_to_rust(b: iroh_ffi_bytes_t) -> Vec<u8> {
    let bytes = if b.ptr.is_null() || b.len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(b.ptr as *const u8, b.len).to_vec()
    };
    iroh_ffi_bytes_free(b);
    bytes
}

async fn wait_op(op: usize) -> PyResult<usize> {
    let op = tokio::task::spawn_blocking(move || unsafe {
        check(iroh_ffi_operation_wait(op_ptr(op)))?;
        Ok(op)
    })
    .await
    .map_err(|e| IrohError::new_err(e.to_string()))
    .and_then(|r| r)?;
    Ok(op)
}

#[pyclass]
#[derive(Clone)]
pub struct NodeAddr {
    #[pyo3(get)]
    pub endpoint_id: String,
    #[pyo3(get)]
    pub relay_url: Option<String>,
    #[pyo3(get)]
    pub direct_addresses: Vec<String>,
}

impl NodeAddr {
    unsafe fn to_ffi(&self) -> iroh_ffi_node_addr_t {
        let mut directs: Vec<iroh_ffi_string_t> = self.direct_addresses.iter().cloned().map(str_to_ffi).collect();
        let len = directs.len();
        let ptr = if directs.is_empty() { ptr::null_mut() } else { let p = directs.as_mut_ptr(); std::mem::forget(directs); p };
        iroh_ffi_node_addr_t {
            endpoint_id: str_to_ffi(self.endpoint_id.clone()),
            relay_url: str_to_ffi(self.relay_url.clone().unwrap_or_default()),
            direct_addresses: ptr,
            direct_addresses_len: len,
        }
    }

    unsafe fn from_ffi(addr: iroh_ffi_node_addr_t) -> Self {
        let endpoint_id = free_string_to_rust(addr.endpoint_id);
        let relay = free_string_to_rust(addr.relay_url);
        let direct_addresses = if addr.direct_addresses.is_null() || addr.direct_addresses_len == 0 {
            Vec::new()
        } else {
            let items = Vec::from_raw_parts(addr.direct_addresses, addr.direct_addresses_len, addr.direct_addresses_len);
            items.into_iter().map(|item| unsafe { free_string_to_rust(item) }).collect()
        };
        Self { endpoint_id, relay_url: if relay.is_empty() { None } else { Some(relay) }, direct_addresses }
    }
}

#[pymethods]
impl NodeAddr {
    #[new]
    #[pyo3(signature = (endpoint_id, relay_url=None, direct_addresses=None))]
    fn new(endpoint_id: String, relay_url: Option<String>, direct_addresses: Option<Vec<String>>) -> Self {
        Self { endpoint_id, relay_url, direct_addresses: direct_addresses.unwrap_or_default() }
    }
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<&'py PyDict> {
        let d = PyDict::new(py);
        d.set_item("endpoint_id", self.endpoint_id.clone())?;
        d.set_item("relay_url", self.relay_url.clone())?;
        d.set_item("direct_addresses", self.direct_addresses.clone())?;
        Ok(d)
    }
    fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<&'py PyBytes> {
        let relay = self.relay_url.clone().unwrap_or_default();
        let direct = self.direct_addresses.join("\n");
        Ok(PyBytes::new(py, format!("{}\n{}\n{}", self.endpoint_id, relay, direct).as_bytes()))
    }
    #[staticmethod]
    fn from_bytes(data: Vec<u8>) -> PyResult<Self> {
        let text = String::from_utf8(data).map_err(|e| IrohError::new_err(e.to_string()))?;
        let mut lines = text.split('\n');
        let endpoint_id = lines.next().unwrap_or_default().to_string();
        let relay_url = match lines.next() { Some("") | None => None, Some(v) => Some(v.to_string()) };
        let direct_addresses = lines.filter(|line| !line.is_empty()).map(str::to_string).collect();
        Ok(Self { endpoint_id, relay_url, direct_addresses })
    }
    #[staticmethod]
    fn from_dict(data: &PyDict) -> PyResult<Self> {
        Ok(Self {
            endpoint_id: data.get_item("endpoint_id")?.ok_or_else(|| IrohError::new_err("missing endpoint_id"))?.extract()?,
            relay_url: match data.get_item("relay_url")? { Some(v) => v.extract::<Option<String>>()?, None => None },
            direct_addresses: match data.get_item("direct_addresses")? { Some(v) => v.extract::<Vec<String>>()?, None => Vec::new() },
        })
    }
}

#[pyclass]
#[derive(Clone)]
pub struct EndpointConfig {
    #[pyo3(get, set)]
    pub relay_mode: Option<String>,
    #[pyo3(get, set)]
    pub alpns: Vec<Vec<u8>>,
    #[pyo3(get, set)]
    pub secret_key: Option<Vec<u8>>,
}

#[pymethods]
impl EndpointConfig {
    #[new]
    #[pyo3(signature = (alpns, relay_mode=None, secret_key=None))]
    fn new(alpns: Vec<Vec<u8>>, relay_mode: Option<String>, secret_key: Option<Vec<u8>>) -> Self {
        Self { relay_mode, alpns, secret_key }
    }
}

macro_rules! impl_handle_wrapper {
    ($name:ident, $raw:ty, $free_fn:ident) => {
        #[pyclass]
        pub struct $name { inner: usize }
        unsafe impl Send for $name {}
        unsafe impl Sync for $name {}
        impl Drop for $name { fn drop(&mut self) { unsafe { $free_fn(usize_to_ptr::<$raw>(self.inner)) } } }
        impl $name {
            fn as_ptr(&self) -> *mut $raw { usize_to_ptr(self.inner) }
            fn from_ptr(ptr: *mut $raw) -> Self { Self { inner: ptr_to_usize(ptr) } }
        }
    };
}

impl_handle_wrapper!(IrohNode, iroh_transport_ffi::NodeHandle, iroh_ffi_node_free);
impl_handle_wrapper!(BlobsClient, iroh_transport_ffi::BlobsHandle, iroh_ffi_blobs_free);
impl_handle_wrapper!(DocsClient, iroh_transport_ffi::DocsHandle, iroh_ffi_docs_free);
impl_handle_wrapper!(DocHandle, iroh_transport_ffi::DocHandle, iroh_ffi_doc_free);
impl_handle_wrapper!(GossipClient, iroh_transport_ffi::GossipHandle, iroh_ffi_gossip_free);
impl_handle_wrapper!(GossipTopicHandle, iroh_transport_ffi::TopicHandle, iroh_ffi_topic_free);
impl_handle_wrapper!(NetClient, iroh_transport_ffi::NetHandle, iroh_ffi_net_free);
impl_handle_wrapper!(IrohConnection, iroh_transport_ffi::ConnectionHandle, iroh_ffi_connection_free);
impl_handle_wrapper!(IrohSendStream, iroh_transport_ffi::SendStreamHandle, iroh_ffi_send_stream_free);
impl_handle_wrapper!(IrohRecvStream, iroh_transport_ffi::RecvStreamHandle, iroh_ffi_recv_stream_free);

#[pymethods]
impl IrohNode {
    #[staticmethod]
    fn memory<'py>(py: Python<'py>) -> PyResult<&'py PyAny> {
        future_into_py(py, async move {
            let mut op = op_slot();
            unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_node_memory(&mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?;
            let mut out = ptr::null_mut();
            unsafe { check(iroh_ffi_operation_take_node(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(IrohNode::from_ptr(out))
        })
    }
    #[staticmethod]
    fn persistent<'py>(py: Python<'py>, path: String) -> PyResult<&'py PyAny> {
        future_into_py(py, async move {
            let path = str_to_ffi(path);
            let mut op = op_slot();
            unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_node_persistent(path, &mut raw))?; iroh_ffi_string_free(path); set_op(&mut op, raw); }
            let op = wait_op(op).await?;
            let mut out = ptr::null_mut();
            unsafe { check(iroh_ffi_operation_take_node(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(IrohNode::from_ptr(out))
        })
    }
    fn node_id(&self) -> PyResult<String> {
        let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
        unsafe { check(iroh_ffi_node_id(self.as_ptr(), &mut out))?; Ok(free_string_to_rust(out)) }
    }
    fn node_addr(&self) -> PyResult<String> {
        let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
        unsafe { check(iroh_ffi_node_addr_debug(self.as_ptr(), &mut out))?; Ok(free_string_to_rust(out)) }
    }
    fn node_addr_info(&self) -> PyResult<NodeAddr> {
        let mut out: iroh_ffi_node_addr_t = unsafe { std::mem::zeroed() };
        unsafe { check(iroh_ffi_node_addr(self.as_ptr(), &mut out))?; Ok(NodeAddr::from_ffi(out)) }
    }
    fn close<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_node_close(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; unsafe { check(iroh_ffi_operation_take_unit(op_ptr(op)))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(())
        })
    }
    fn shutdown<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> { self.close(py) }
    fn add_node_addr(&self, other: &IrohNode) -> PyResult<()> { unsafe { check(iroh_ffi_node_add_node_addr(self.as_ptr(), other.as_ptr())) } }
}

#[pyfunction]
fn blobs_client(node: &IrohNode) -> PyResult<BlobsClient> {
    let mut out = ptr::null_mut();
    unsafe { check(iroh_ffi_node_blobs_client(node.as_ptr(), &mut out))?; }
    Ok(BlobsClient::from_ptr(out))
}

#[pyfunction]
fn docs_client(node: &IrohNode) -> PyResult<DocsClient> {
    let mut out = ptr::null_mut();
    unsafe { check(iroh_ffi_node_docs_client(node.as_ptr(), &mut out))?; }
    Ok(DocsClient::from_ptr(out))
}

#[pyfunction]
fn gossip_client(node: &IrohNode) -> PyResult<GossipClient> {
    let mut out = ptr::null_mut();
    unsafe { check(iroh_ffi_node_gossip_client(node.as_ptr(), &mut out))?; }
    Ok(GossipClient::from_ptr(out))
}

#[pyfunction]
fn net_client(node: &IrohNode) -> PyResult<NetClient> {
    let mut out = ptr::null_mut();
    unsafe { check(iroh_ffi_node_net_client(node.as_ptr(), &mut out))?; }
    Ok(NetClient::from_ptr(out))
}

#[pymethods]
impl BlobsClient {
    fn add_bytes<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let data = bytes_to_ffi(data);
            let mut op = op_slot();
            unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_blobs_add_bytes(usize_to_ptr(inner), data, &mut raw))?; iroh_ffi_bytes_free(data); set_op(&mut op, raw); }
            let op = wait_op(op).await?;
            let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
            unsafe { check(iroh_ffi_operation_take_string(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); Ok(free_string_to_rust(out)) }
        })
    }
    fn read_to_bytes<'py>(&self, py: Python<'py>, hash_hex: String) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py::<_, PyObject>(py, async move {
            let hash = str_to_ffi(hash_hex);
            let mut op = op_slot();
            unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_blobs_read_to_bytes(usize_to_ptr(inner), hash, &mut raw))?; iroh_ffi_string_free(hash); set_op(&mut op, raw); }
            let op = wait_op(op).await?;
            let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 };
            unsafe { check(iroh_ffi_operation_take_bytes(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); let bytes = free_bytes_to_rust(out); Ok(Python::with_gil(|py| PyBytes::new(py, &bytes).into_py(py))) }
        })
    }
    fn create_ticket(&self, hash_hex: String) -> PyResult<String> {
        let hash = str_to_ffi(hash_hex);
        let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
        unsafe { check(iroh_ffi_blobs_create_ticket(self.as_ptr(), hash, &mut out))?; iroh_ffi_string_free(hash); Ok(free_string_to_rust(out)) }
    }
    fn download_blob<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py::<_, PyObject>(py, async move {
            let ticket = str_to_ffi(ticket_str);
            let mut op = op_slot();
            unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_blobs_download_blob(usize_to_ptr(inner), ticket, &mut raw))?; iroh_ffi_string_free(ticket); set_op(&mut op, raw); }
            let op = wait_op(op).await?;
            let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 };
            unsafe { check(iroh_ffi_operation_take_bytes(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); let bytes = free_bytes_to_rust(out); Ok(Python::with_gil(|py| PyBytes::new(py, &bytes).into_py(py))) }
        })
    }
}

#[pymethods]
impl DocsClient {
    fn create<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_docs_create(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_doc(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(DocHandle::from_ptr(out))
        })
    }
    fn create_author<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_docs_create_author(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_operation_take_string(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); Ok(free_string_to_rust(out)) }
        })
    }
    fn join<'py>(&self, py: Python<'py>, ticket_str: String) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let ticket = str_to_ffi(ticket_str);
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_docs_join(usize_to_ptr(inner), ticket, &mut raw))?; iroh_ffi_string_free(ticket); set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_doc(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(DocHandle::from_ptr(out))
        })
    }
}

#[pymethods]
impl DocHandle {
    fn doc_id(&self) -> PyResult<String> {
        let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
        unsafe { check(iroh_ffi_doc_id(self.as_ptr(), &mut out))?; Ok(free_string_to_rust(out)) }
    }
    fn set_bytes<'py>(&self, py: Python<'py>, author_hex: String, key: Vec<u8>, value: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let author = str_to_ffi(author_hex); let key = bytes_to_ffi(key); let value = bytes_to_ffi(value);
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_doc_set_bytes(usize_to_ptr(inner), author, key, value, &mut raw))?; iroh_ffi_string_free(author); iroh_ffi_bytes_free(key); iroh_ffi_bytes_free(value); set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_operation_take_string(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); Ok(free_string_to_rust(out)) }
        })
    }
    fn get_exact<'py>(&self, py: Python<'py>, author_hex: String, key: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let author = str_to_ffi(author_hex); let key = bytes_to_ffi(key);
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_doc_get_exact(usize_to_ptr(inner), author, key, &mut raw))?; iroh_ffi_string_free(author); iroh_ffi_bytes_free(key); set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut present = false; let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 };
            unsafe {
                check(iroh_ffi_operation_take_optional_bytes(op_ptr(op), &mut present, &mut out))?; iroh_ffi_operation_free(op_ptr(op));
                Ok(Python::with_gil(|py| if present { let bytes = free_bytes_to_rust(out); PyBytes::new(py, &bytes).into_py(py) } else { iroh_ffi_bytes_free(out); py.None() }))
            }
        })
    }
    fn share<'py>(&self, py: Python<'py>, mode: String, _endpoint: &IrohNode) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mode = str_to_ffi(mode);
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_doc_share(usize_to_ptr(inner), mode, &mut raw))?; iroh_ffi_string_free(mode); set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_operation_take_string(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); Ok(free_string_to_rust(out)) }
        })
    }
}

#[pymethods]
impl GossipClient {
    fn subscribe<'py>(&self, py: Python<'py>, topic_bytes: Vec<u8>, bootstrap_peers: Vec<String>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let op = start_gossip_subscribe_op(inner, topic_bytes, bootstrap_peers)?;
            let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_topic(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(GossipTopicHandle::from_ptr(out))
        })
    }
}

#[pymethods]
impl GossipTopicHandle {
    fn broadcast<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let data = bytes_to_ffi(data); let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_topic_broadcast(usize_to_ptr(inner), data, &mut raw))?; iroh_ffi_bytes_free(data); set_op(&mut op, raw); }
            let op = wait_op(op).await?; unsafe { check(iroh_ffi_operation_take_unit(op_ptr(op)))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(())
        })
    }
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_topic_recv(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out: iroh_ffi_gossip_event_t = unsafe { std::mem::zeroed() };
            unsafe {
                check(iroh_ffi_operation_take_gossip_event(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op));
                let et = free_string_to_rust(out.event_type);
                let data: Option<PyObject> = if out.data_present { let bytes = free_bytes_to_rust(out.data); Some(Python::with_gil(|py| PyBytes::new(py, &bytes).into_py(py))) } else { iroh_ffi_bytes_free(out.data); None };
                Ok((et, data))
            }
        })
    }
}

#[pyfunction]
fn create_endpoint<'py>(py: Python<'py>, alpn: Vec<u8>) -> PyResult<&'py PyAny> {
    future_into_py(py, async move {
        let alpn = bytes_to_ffi(alpn); let mut op = op_slot();
        unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_endpoint_create(alpn, &mut raw))?; iroh_ffi_bytes_free(alpn); set_op(&mut op, raw); }
        let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_net(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
        Ok(NetClient::from_ptr(out))
    })
}

#[pyfunction]
fn create_endpoint_with_config<'py>(py: Python<'py>, config: EndpointConfig) -> PyResult<&'py PyAny> {
    let alpn = config.alpns.into_iter().next().ok_or_else(|| IrohError::new_err("alpns must not be empty"))?;
    create_endpoint(py, alpn)
}

#[pymethods]
impl NetClient {
    fn connect<'py>(&self, py: Python<'py>, node_id: String, alpn: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let node_id = str_to_ffi(node_id); let alpn = bytes_to_ffi(alpn); let mut op = op_slot();
            unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_net_connect(usize_to_ptr(inner), node_id, alpn, &mut raw))?; iroh_ffi_string_free(node_id); iroh_ffi_bytes_free(alpn); set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_connection(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(IrohConnection::from_ptr(out))
        })
    }
    fn connect_node_addr<'py>(&self, py: Python<'py>, addr: NodeAddr, alpn: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let op = start_net_connect_node_addr_op(inner, addr, alpn)?;
            let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_connection(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(IrohConnection::from_ptr(out))
        })
    }
    fn accept<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_net_accept(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_connection(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); }
            Ok(IrohConnection::from_ptr(out))
        })
    }
    fn endpoint_id(&self) -> PyResult<String> {
        let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
        unsafe { check(iroh_ffi_net_endpoint_id(self.as_ptr(), &mut out))?; Ok(free_string_to_rust(out)) }
    }
    fn endpoint_addr(&self) -> PyResult<String> {
        let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 };
        unsafe { check(iroh_ffi_net_endpoint_addr_debug(self.as_ptr(), &mut out))?; Ok(free_string_to_rust(out)) }
    }
    fn endpoint_addr_info(&self) -> PyResult<NodeAddr> {
        let mut out: iroh_ffi_node_addr_t = unsafe { std::mem::zeroed() };
        unsafe { check(iroh_ffi_net_endpoint_addr(self.as_ptr(), &mut out))?; Ok(NodeAddr::from_ffi(out)) }
    }
    fn close<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_net_close(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; unsafe { check(iroh_ffi_operation_take_unit(op_ptr(op)))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(()) })
    }
    fn closed<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_net_closed(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; unsafe { check(iroh_ffi_operation_take_unit(op_ptr(op)))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(()) })
    }
}

#[pymethods]
impl IrohConnection {
    fn open_bi<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_connection_open_bi(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let (mut s, mut r) = (ptr::null_mut(), ptr::null_mut()); unsafe { check(iroh_ffi_operation_take_bi_streams(op_ptr(op), &mut s, &mut r))?; iroh_ffi_operation_free(op_ptr(op)); } Ok((IrohSendStream::from_ptr(s), IrohRecvStream::from_ptr(r))) })
    }
    fn accept_bi<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_connection_accept_bi(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let (mut s, mut r) = (ptr::null_mut(), ptr::null_mut()); unsafe { check(iroh_ffi_operation_take_bi_streams(op_ptr(op), &mut s, &mut r))?; iroh_ffi_operation_free(op_ptr(op)); } Ok((IrohSendStream::from_ptr(s), IrohRecvStream::from_ptr(r))) })
    }
    fn open_uni<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_connection_open_uni(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let mut s = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_send_stream(op_ptr(op), &mut s))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(IrohSendStream::from_ptr(s)) })
    }
    fn accept_uni<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_connection_accept_uni(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let mut r = ptr::null_mut(); unsafe { check(iroh_ffi_operation_take_recv_stream(op_ptr(op), &mut r))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(IrohRecvStream::from_ptr(r)) })
    }
    fn send_datagram(&self, data: Vec<u8>) -> PyResult<()> { let data = bytes_to_ffi(data); unsafe { let res = check(iroh_ffi_connection_send_datagram(self.as_ptr(), data)); iroh_ffi_bytes_free(data); res } }
    fn read_datagram<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py::<_, PyObject>(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_connection_read_datagram(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_operation_take_bytes(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); let bytes = free_bytes_to_rust(out); Ok(Python::with_gil(|py| PyBytes::new(py, &bytes).into_py(py))) } })
    }
    fn remote_id(&self) -> PyResult<String> { let mut out = iroh_ffi_string_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_connection_remote_id(self.as_ptr(), &mut out))?; Ok(free_string_to_rust(out)) } }
    fn close(&self, code: u64, reason: Vec<u8>) -> PyResult<()> { let reason = bytes_to_ffi(reason); unsafe { let res = check(iroh_ffi_connection_close(self.as_ptr(), code, reason)); iroh_ffi_bytes_free(reason); res } }
    fn closed<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_connection_closed(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut out: iroh_ffi_closed_info_t = unsafe { std::mem::zeroed() };
            unsafe {
                check(iroh_ffi_operation_take_closed_info(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op));
                let kind = free_string_to_rust(out.kind); let reason = free_bytes_to_rust(out.reason);
                let result = Python::with_gil(|py| -> PyResult<PyObject> {
                    let d = PyDict::new(py);
                    d.set_item("kind", kind)?;
                    d.set_item("code", if out.code_present { Some(out.code) } else { None::<u64> })?;
                    d.set_item("reason", if reason.is_empty() { None::<PyObject> } else { Some(PyBytes::new(py, &reason).into_py(py)) })?;
                    Ok(d.into_py(py))
                })?;
                Ok(result)
            }
        })
    }
}

#[pymethods]
impl IrohSendStream {
    fn write_all<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let data = bytes_to_ffi(data); let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_send_stream_write_all(usize_to_ptr(inner), data, &mut raw))?; iroh_ffi_bytes_free(data); set_op(&mut op, raw); } let op = wait_op(op).await?; unsafe { check(iroh_ffi_operation_take_unit(op_ptr(op)))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(()) })
    }
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_send_stream_finish(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; unsafe { check(iroh_ffi_operation_take_unit(op_ptr(op)))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(()) })
    }
    fn stopped<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_send_stream_stopped(usize_to_ptr(inner), &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let mut present = false; let mut out = 0u64; unsafe { check(iroh_ffi_operation_take_optional_u64(op_ptr(op), &mut present, &mut out))?; iroh_ffi_operation_free(op_ptr(op)); } Ok(if present { Some(out) } else { None }) })
    }
}

#[pymethods]
impl IrohRecvStream {
    fn read<'py>(&self, py: Python<'py>, max_len: usize) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py::<_, PyObject>(py, async move {
            let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_recv_stream_read(usize_to_ptr(inner), max_len, &mut raw))?; set_op(&mut op, raw); }
            let op = wait_op(op).await?; let mut present = false; let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 };
            unsafe { check(iroh_ffi_operation_take_optional_bytes(op_ptr(op), &mut present, &mut out))?; iroh_ffi_operation_free(op_ptr(op)); Ok(Python::with_gil(|py| if present { let bytes = free_bytes_to_rust(out); PyBytes::new(py, &bytes).into_py(py) } else { iroh_ffi_bytes_free(out); py.None() })) }
        })
    }
    fn read_exact<'py>(&self, py: Python<'py>, n: usize) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py::<_, PyObject>(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_recv_stream_read_exact(usize_to_ptr(inner), n, &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_operation_take_bytes(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); let bytes = free_bytes_to_rust(out); Ok(Python::with_gil(|py| PyBytes::new(py, &bytes).into_py(py))) } })
    }
    fn read_to_end<'py>(&self, py: Python<'py>, max_size: usize) -> PyResult<&'py PyAny> {
        let inner = self.inner;
        future_into_py::<_, PyObject>(py, async move { let mut op = op_slot(); unsafe { let mut raw = ptr::null_mut(); check(iroh_ffi_recv_stream_read_to_end(usize_to_ptr(inner), max_size, &mut raw))?; set_op(&mut op, raw); } let op = wait_op(op).await?; let mut out = iroh_ffi_bytes_t { ptr: ptr::null_mut(), len: 0 }; unsafe { check(iroh_ffi_operation_take_bytes(op_ptr(op), &mut out))?; iroh_ffi_operation_free(op_ptr(op)); let bytes = free_bytes_to_rust(out); Ok(Python::with_gil(|py| PyBytes::new(py, &bytes).into_py(py))) } })
    }
    fn stop(&self, code: u64) -> PyResult<()> { unsafe { check(iroh_ffi_recv_stream_stop(self.as_ptr(), code)) } }
}

#[pymodule]
fn _iroh_python(py: Python, m: &PyModule) -> PyResult<()> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    pyo3_asyncio::tokio::init(builder);

    m.add("IrohError", py.get_type::<IrohError>())?;
    m.add_class::<NodeAddr>()?;
    m.add_class::<EndpointConfig>()?;
    m.add_class::<IrohNode>()?;
    m.add_class::<BlobsClient>()?;
    m.add_class::<DocsClient>()?;
    m.add_class::<DocHandle>()?;
    m.add_class::<GossipClient>()?;
    m.add_class::<GossipTopicHandle>()?;
    m.add_class::<NetClient>()?;
    m.add_class::<IrohConnection>()?;
    m.add_class::<IrohSendStream>()?;
    m.add_class::<IrohRecvStream>()?;
    m.add_function(wrap_pyfunction!(blobs_client, m)?)?;
    m.add_function(wrap_pyfunction!(docs_client, m)?)?;
    m.add_function(wrap_pyfunction!(gossip_client, m)?)?;
    m.add_function(wrap_pyfunction!(net_client, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint_with_config, m)?)?;
    Ok(())
}
