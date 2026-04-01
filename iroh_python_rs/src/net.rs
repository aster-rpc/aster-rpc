//! Network module - wraps CoreNetClient, CoreConnection from iroh_transport_core.
//!
//! Phase 2: Now wraps iroh_transport_core types instead of iroh types directly.
//! Phase 1b surfaces: max_datagram_size, datagram_send_buffer_space, connection_info.

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_async_runtimes::tokio::future_into_py;

use iroh_transport_core::{
    ConnectionType, ConnectionTypeDetail, CoreConnection, CoreConnectionInfo, CoreEndpointConfig,
    CoreNetClient, CoreNodeAddr, CoreRemoteInfo, CoreSendStream, CoreRecvStream,
};

use crate::error::err_to_py;
use crate::node::IrohNode;
use crate::PyBytesResult;

// ============================================================================
// Shared Types
// ============================================================================

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

impl From<CoreNodeAddr> for NodeAddr {
    fn from(addr: CoreNodeAddr) -> Self {
        Self {
            endpoint_id: addr.endpoint_id,
            relay_url: addr.relay_url,
            direct_addresses: addr.direct_addresses,
        }
    }
}

#[pymethods]
impl NodeAddr {
    #[new]
    #[pyo3(signature = (endpoint_id, relay_url=None, direct_addresses=None))]
    fn new(
        endpoint_id: String,
        relay_url: Option<String>,
        direct_addresses: Option<Vec<String>>,
    ) -> Self {
        Self {
            endpoint_id,
            relay_url,
            direct_addresses: direct_addresses.unwrap_or_default(),
        }
    }

    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("endpoint_id", self.endpoint_id.clone())?;
        d.set_item("relay_url", self.relay_url.clone())?;
        d.set_item("direct_addresses", self.direct_addresses.clone())?;
        Ok(d)
    }

    fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let relay = self.relay_url.clone().unwrap_or_default();
        let direct = self.direct_addresses.join("\n");
        let encoded = format!("{}\n{}\n{}", self.endpoint_id, relay, direct);
        Ok(PyBytes::new(py, encoded.as_bytes()))
    }

    #[staticmethod]
    fn from_bytes(data: Vec<u8>) -> PyResult<Self> {
        let text = String::from_utf8(data).map_err(err_to_py)?;
        let mut lines = text.split('\n');
        let endpoint_id = lines
            .next()
            .ok_or_else(|| err_to_py("missing endpoint_id"))?
            .to_string();
        let relay_url = match lines.next() {
            Some("") | None => None,
            Some(v) => Some(v.to_string()),
        };
        let direct_addresses = lines
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Self {
            endpoint_id,
            relay_url,
            direct_addresses,
        })
    }

    #[staticmethod]
    fn from_dict(data: &Bound<'_, PyDict>) -> PyResult<Self> {
        let endpoint_id = data
            .get_item("endpoint_id")?
            .ok_or_else(|| err_to_py("missing endpoint_id"))?
            .extract::<String>()?;
        let relay_url = match data.get_item("relay_url")? {
            Some(v) => v.extract::<Option<String>>()?,
            None => None,
        };
        let direct_addresses = match data.get_item("direct_addresses")? {
            Some(v) => v.extract::<Vec<String>>()?,
            None => Vec::new(),
        };
        Ok(Self {
            endpoint_id,
            relay_url,
            direct_addresses,
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
    /// Enable connection monitoring / remote-info tracking (Phase 1b)
    #[pyo3(get, set)]
    pub enable_monitoring: bool,
    /// Enable endpoint hooks (Phase 1b)
    #[pyo3(get, set)]
    pub enable_hooks: bool,
    /// Timeout in ms for hook replies (default 5000)
    #[pyo3(get, set)]
    pub hook_timeout_ms: u64,
}

#[pymethods]
impl EndpointConfig {
    #[new]
    #[pyo3(signature = (alpns, relay_mode=None, secret_key=None, enable_monitoring=false, enable_hooks=false, hook_timeout_ms=5000))]
    fn new(
        alpns: Vec<Vec<u8>>,
        relay_mode: Option<String>,
        secret_key: Option<Vec<u8>>,
        enable_monitoring: bool,
        enable_hooks: bool,
        hook_timeout_ms: u64,
    ) -> Self {
        Self {
            relay_mode,
            alpns,
            secret_key,
            enable_monitoring,
            enable_hooks,
            hook_timeout_ms,
        }
    }
}

impl From<&EndpointConfig> for CoreEndpointConfig {
    fn from(config: &EndpointConfig) -> Self {
        CoreEndpointConfig {
            relay_mode: config.relay_mode.clone(),
            relay_urls: Vec::new(),
            alpns: config.alpns.clone(),
            secret_key: config.secret_key.clone(),
            enable_discovery: true,
            enable_monitoring: config.enable_monitoring,
            enable_hooks: config.enable_hooks,
            hook_timeout_ms: config.hook_timeout_ms,
        }
    }
}

// ============================================================================
// Phase 1b: ConnectionInfo (exposed via CoreConnection.connection_info())
// ============================================================================

/// Python representation of connection type
#[pyclass]
#[derive(Clone)]
pub struct ConnectionInfo {
    #[pyo3(get)]
    pub connection_type: String,
    #[pyo3(get)]
    pub bytes_sent: u64,
    #[pyo3(get)]
    pub bytes_received: u64,
    #[pyo3(get)]
    pub rtt_ns: Option<u64>,
    #[pyo3(get)]
    pub alpn: Vec<u8>,
    #[pyo3(get)]
    pub is_connected: bool,
}

impl From<CoreConnectionInfo> for ConnectionInfo {
    fn from(info: CoreConnectionInfo) -> Self {
        let connection_type = match info.connection_type {
            ConnectionTypeDetail::UdpDirect => "udp_direct".to_string(),
            ConnectionTypeDetail::UdpRelay => "udp_relay".to_string(),
            ConnectionTypeDetail::Other(s) => s,
        };
        Self {
            connection_type,
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
            rtt_ns: info.rtt_ns,
            alpn: info.alpn,
            is_connected: info.is_connected,
        }
    }
}

#[pyclass]
#[derive(Clone)]
pub struct RemoteInfo {
    #[pyo3(get)]
    pub node_id: String,
    #[pyo3(get)]
    pub relay_url: Option<String>,
    #[pyo3(get)]
    pub connection_type: String,
    #[pyo3(get)]
    pub last_handshake_ns: Option<u64>,
    #[pyo3(get)]
    pub bytes_sent: u64,
    #[pyo3(get)]
    pub bytes_received: u64,
    #[pyo3(get)]
    pub is_connected: bool,
}

impl From<CoreRemoteInfo> for RemoteInfo {
    fn from(info: CoreRemoteInfo) -> Self {
        let connection_type = match info.connection_type {
            ConnectionType::NotConnected => "not_connected".to_string(),
            ConnectionType::Connecting => "connecting".to_string(),
            ConnectionType::Connected(detail) => match detail {
                ConnectionTypeDetail::UdpDirect => "udp_direct".to_string(),
                ConnectionTypeDetail::UdpRelay => "udp_relay".to_string(),
                ConnectionTypeDetail::Other(s) => s,
            },
        };
        Self {
            node_id: info.node_id,
            relay_url: info.relay_url,
            connection_type,
            last_handshake_ns: info.last_handshake_ns,
            bytes_sent: info.bytes_sent,
            bytes_received: info.bytes_received,
            is_connected: info.is_connected,
        }
    }
}

// ============================================================================
// ClosedResult — returned by connection.closed()
// ============================================================================

/// Result of waiting for a connection to close.
/// Provides kind, code, and optional reason as a dict-like object.
pub(crate) struct ClosedResult {
    pub kind: String,
    pub code: Option<u64>,
    pub reason: Option<Vec<u8>>,
}

impl<'py> IntoPyObject<'py> for ClosedResult {
    type Target = PyDict;
    type Output = Bound<'py, PyDict>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        let d = PyDict::new(py);
        d.set_item("kind", self.kind)?;
        d.set_item("code", self.code)?;
        d.set_item("reason", self.reason.map(PyBytesResult))?;
        Ok(d)
    }
}

// ============================================================================
// SendStream wrapper
// ============================================================================

#[pyclass]
pub struct IrohSendStream {
    inner: CoreSendStream,
}

impl From<CoreSendStream> for IrohSendStream {
    fn from(inner: CoreSendStream) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl IrohSendStream {
    /// Write all bytes to the stream.
    fn write_all<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.inner.clone();
        future_into_py(py, async move {
            stream.write_all(data).await.map_err(err_to_py)?;
            Ok(())
        })
    }

    /// Signal that no more data will be written.
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.inner.clone();
        future_into_py(py, async move {
            stream.finish().await.map_err(err_to_py)?;
            Ok(())
        })
    }

    fn stopped<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.inner.clone();
        future_into_py(py, async move {
            let code = stream.stopped().await.map_err(err_to_py)?;
            Ok(code)
        })
    }
}

// ============================================================================
// RecvStream wrapper
// ============================================================================

#[pyclass]
pub struct IrohRecvStream {
    inner: CoreRecvStream,
}

impl From<CoreRecvStream> for IrohRecvStream {
    fn from(inner: CoreRecvStream) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl IrohRecvStream {
    fn read<'py>(&self, py: Python<'py>, max_len: usize) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.inner.clone();
        future_into_py(py, async move {
            let chunk = stream.read(max_len).await.map_err(err_to_py)?;
            Ok(chunk.map(PyBytesResult))
        })
    }

    fn read_exact<'py>(&self, py: Python<'py>, n: usize) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.inner.clone();
        future_into_py(py, async move {
            let data = stream.read_exact(n).await.map_err(err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    /// Read all remaining data up to `max_size` bytes.
    fn read_to_end<'py>(&self, py: Python<'py>, max_size: usize) -> PyResult<Bound<'py, PyAny>> {
        let stream = self.inner.clone();
        future_into_py(py, async move {
            let data = stream.read_to_end(max_size).await.map_err(err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    fn stop(&self, code: u64) -> PyResult<()> {
        self.inner.stop(code).map_err(err_to_py)
    }
}

// ============================================================================
// Connection wrapper
// ============================================================================

#[pyclass]
pub struct IrohConnection {
    inner: CoreConnection,
}

impl From<CoreConnection> for IrohConnection {
    fn from(inner: CoreConnection) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl IrohConnection {
    /// Open a bidirectional QUIC stream, returning (send, recv).
    fn open_bi<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let (send, recv) = conn.open_bi().await.map_err(err_to_py)?;
            Ok((IrohSendStream::from(send), IrohRecvStream::from(recv)))
        })
    }

    fn open_uni<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let send = conn.open_uni().await.map_err(err_to_py)?;
            Ok(IrohSendStream::from(send))
        })
    }

    /// Accept an incoming bidirectional stream from the peer.
    fn accept_bi<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let (send, recv) = conn.accept_bi().await.map_err(err_to_py)?;
            Ok((IrohSendStream::from(send), IrohRecvStream::from(recv)))
        })
    }

    fn accept_uni<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let recv = conn.accept_uni().await.map_err(err_to_py)?;
            Ok(IrohRecvStream::from(recv))
        })
    }

    /// Send an unreliable datagram over this connection.
    fn send_datagram(&self, data: Vec<u8>) -> PyResult<()> {
        self.inner.send_datagram(data).map_err(err_to_py)
    }

    /// Read the next datagram received on this connection.
    fn read_datagram<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let data = conn.read_datagram().await.map_err(err_to_py)?;
            Ok(PyBytesResult(data))
        })
    }

    /// Return the remote endpoint's ID as a string.
    fn remote_id(&self) -> String {
        self.inner.remote_id()
    }

    fn close(&self, code: u64, reason: Vec<u8>) -> PyResult<()> {
        self.inner.close(code, reason).map_err(err_to_py)
    }

    fn closed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let closed = conn.closed().await;
            Ok(ClosedResult {
                kind: closed.kind,
                code: closed.code,
                reason: closed.reason,
            })
        })
    }

    // ========================================================================
    // Phase 1b: Datagram Completion
    // ========================================================================

    /// Return the maximum datagram size for this connection.
    /// Returns None if datagrams are not supported.
    fn max_datagram_size(&self) -> Option<usize> {
        self.inner.max_datagram_size()
    }

    /// Return the available buffer space for datagram sends.
    fn datagram_send_buffer_space(&self) -> usize {
        self.inner.datagram_send_buffer_space()
    }

    // ========================================================================
    // Phase 1b: Connection Info
    // ========================================================================

    /// Return information about this connection.
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::from(self.inner.connection_info())
    }
}

// ============================================================================
// NetClient — wraps CoreNetClient for connecting / accepting
// ============================================================================

#[pyclass]
pub struct NetClient {
    inner: CoreNetClient,
}

impl From<CoreNetClient> for NetClient {
    fn from(inner: CoreNetClient) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl NetClient {
    /// Connect to a remote node by its endpoint ID string and ALPN.
    fn connect<'py>(
        &self,
        py: Python<'py>,
        node_id: String,
        alpn: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let conn = client.connect(node_id, alpn).await.map_err(err_to_py)?;
            Ok(IrohConnection::from(conn))
        })
    }

    fn connect_node_addr<'py>(
        &self,
        py: Python<'py>,
        addr: NodeAddr,
        alpn: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        let core_addr = CoreNodeAddr {
            endpoint_id: addr.endpoint_id,
            relay_url: addr.relay_url,
            direct_addresses: addr.direct_addresses,
        };
        future_into_py(py, async move {
            let conn = client.connect_node_addr(core_addr, alpn).await.map_err(err_to_py)?;
            Ok(IrohConnection::from(conn))
        })
    }

    /// Accept one incoming connection (only works on bare endpoints, not IrohNode).
    fn accept<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            let conn = client.accept().await.map_err(err_to_py)?;
            Ok(IrohConnection::from(conn))
        })
    }

    /// Return this endpoint's ID as a hex string.
    fn endpoint_id(&self) -> String {
        self.inner.endpoint_id()
    }

    /// Return the endpoint's address info (debug format).
    fn endpoint_addr(&self) -> String {
        format!("{:?}", self.inner.endpoint_addr_info())
    }

    fn endpoint_addr_info(&self) -> NodeAddr {
        NodeAddr::from(self.inner.endpoint_addr_info())
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.close().await;
            Ok(())
        })
    }

    fn closed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let client = self.inner.clone();
        future_into_py(py, async move {
            client.closed().await;
            Ok(())
        })
    }

    /// Export the endpoint's secret key as 32 bytes.
    fn export_secret_key(&self) -> Vec<u8> {
        self.inner.export_secret_key()
    }

    // ========================================================================
    // Phase 1b: Remote-Info & Monitoring
    // ========================================================================

    /// Query information about a specific known remote endpoint.
    /// Returns None if monitoring is disabled or the remote is unknown.
    fn remote_info(&self, node_id: String) -> Option<RemoteInfo> {
        self.inner.remote_info(&node_id).map(RemoteInfo::from)
    }

    /// Get information about all known remote endpoints.
    /// Returns an empty list if monitoring is disabled.
    fn remote_info_list(&self) -> Vec<RemoteInfo> {
        self.inner
            .remote_info_iter()
            .into_iter()
            .map(RemoteInfo::from)
            .collect()
    }

    /// Returns whether monitoring is enabled for this endpoint.
    fn has_monitoring(&self) -> bool {
        self.inner.has_monitoring()
    }

    /// Returns whether hooks are enabled for this endpoint.
    fn has_hooks(&self) -> bool {
        self.inner.has_hooks()
    }
}

// ============================================================================
// Factory functions
// ============================================================================

/// Get a NetClient backed by an IrohNode's endpoint.
#[pyfunction]
pub fn net_client(node: &IrohNode) -> NetClient {
    NetClient::from(node.inner().net_client())
}

/// Create a bare QUIC endpoint with custom config for custom QUIC usage.
/// Supports both connect and accept.
#[pyfunction]
pub fn create_endpoint<'py>(py: Python<'py>, alpn: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
    future_into_py(py, async move {
        let client = CoreNetClient::create(alpn).await.map_err(err_to_py)?;
        Ok(NetClient::from(client))
    })
}

#[pyfunction]
pub fn create_endpoint_with_config<'py>(
    py: Python<'py>,
    config: EndpointConfig,
) -> PyResult<Bound<'py, PyAny>> {
    let config = CoreEndpointConfig::from(&config);
    future_into_py(py, async move {
        let client = CoreNetClient::create_with_config(config)
            .await
            .map_err(err_to_py)?;
        Ok(NetClient::from(client))
    })
}

// ============================================================================
// Module registration
// ============================================================================

pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NodeAddr>()?;
    m.add_class::<EndpointConfig>()?;
    m.add_class::<ConnectionInfo>()?;
    m.add_class::<RemoteInfo>()?;
    m.add_class::<NetClient>()?;
    m.add_class::<IrohConnection>()?;
    m.add_class::<IrohSendStream>()?;
    m.add_class::<IrohRecvStream>()?;
    m.add_function(wrap_pyfunction!(net_client, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint_with_config, m)?)?;
    Ok(())
}