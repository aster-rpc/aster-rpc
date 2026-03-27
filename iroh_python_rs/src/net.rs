use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

use bytes::Bytes;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3_asyncio::tokio::future_into_py;

use iroh::endpoint::{presets, Connection, ConnectionError, Endpoint, RelayMode, VarInt};
use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey, TransportAddr};

use crate::error::err_to_py;
use crate::node::IrohNode;

fn u64_to_varint(value: u64) -> PyResult<VarInt> {
    VarInt::try_from(value).map_err(err_to_py)
}

pub(crate) fn endpoint_addr_to_py(addr: EndpointAddr) -> NodeAddr {
    let endpoint_id = addr.id.to_string();
    let relay_url = addr.relay_urls().next().map(|url| url.to_string());
    let direct_addresses = addr.ip_addrs().map(|addr| addr.to_string()).collect();
    NodeAddr {
        endpoint_id,
        relay_url,
        direct_addresses,
    }
}

fn py_to_endpoint_addr(addr: &NodeAddr) -> PyResult<EndpointAddr> {
    let id: EndpointId = addr.endpoint_id.parse().map_err(err_to_py)?;
    let mut addrs: Vec<TransportAddr> = addr
        .direct_addresses
        .iter()
        .map(|addr| {
            addr.parse::<SocketAddr>()
                .map(TransportAddr::Ip)
                .map_err(err_to_py)
        })
        .collect::<PyResult<Vec<_>>>()?;
    if let Some(relay_url) = &addr.relay_url {
        addrs.push(TransportAddr::Relay(
            relay_url.parse::<RelayUrl>().map_err(err_to_py)?,
        ));
    }
    Ok(EndpointAddr::from_parts(id, addrs))
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
    fn from_dict(data: &PyDict) -> PyResult<Self> {
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
}

#[pymethods]
impl EndpointConfig {
    #[new]
    #[pyo3(signature = (alpns, relay_mode=None, secret_key=None))]
    fn new(alpns: Vec<Vec<u8>>, relay_mode: Option<String>, secret_key: Option<Vec<u8>>) -> Self {
        Self {
            relay_mode,
            alpns,
            secret_key,
        }
    }
}

fn build_endpoint_config(config: EndpointConfig) -> PyResult<iroh::endpoint::Builder> {
    let mut builder = Endpoint::builder(presets::N0).alpns(config.alpns);
    let relay_mode = match config.relay_mode.as_deref() {
        None | Some("default") => RelayMode::Default,
        Some("disabled") => RelayMode::Disabled,
        Some("staging") => RelayMode::Staging,
        Some(other) => return Err(err_to_py(format!("unsupported relay_mode: {other}"))),
    };
    builder = builder.relay_mode(relay_mode);
    if let Some(secret_key) = config.secret_key {
        let bytes: [u8; 32] = secret_key
            .try_into()
            .map_err(|_| err_to_py("secret_key must be exactly 32 bytes"))?;
        builder = builder.secret_key(SecretKey::from_bytes(&bytes));
    }
    Ok(builder)
}

fn relay_mode_from_config(config: &EndpointConfig) -> PyResult<RelayMode> {
    match config.relay_mode.as_deref() {
        None | Some("default") => Ok(RelayMode::Default),
        Some("disabled") => Ok(RelayMode::Disabled),
        Some("staging") => Ok(RelayMode::Staging),
        Some(other) => Err(err_to_py(format!("unsupported relay_mode: {other}"))),
    }
}

// ---------------------------------------------------------------------------
// SendStream wrapper
// ---------------------------------------------------------------------------

#[pyclass]
pub struct IrohSendStream {
    inner: Arc<Mutex<iroh::endpoint::SendStream>>,
}

#[pymethods]
impl IrohSendStream {
    /// Write all bytes to the stream.
    fn write_all<'py>(&self, py: Python<'py>, data: Vec<u8>) -> PyResult<&'py PyAny> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut stream = inner.lock().await;
            stream.write_all(&data).await.map_err(err_to_py)?;
            Ok(())
        })
    }

    /// Signal that no more data will be written.
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut stream = inner.lock().await;
            stream.finish().map_err(err_to_py)?;
            Ok(())
        })
    }

    fn stopped<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let stream = &mut *inner.lock().await;
            let code = stream.stopped().await.map_err(err_to_py)?;
            Ok(code.map(|v| u64::from(v.into_inner())))
        })
    }
}

// ---------------------------------------------------------------------------
// RecvStream wrapper
// ---------------------------------------------------------------------------

#[pyclass]
pub struct IrohRecvStream {
    inner: Arc<Mutex<iroh::endpoint::RecvStream>>,
}

#[pymethods]
impl IrohRecvStream {
    fn read<'py>(&self, py: Python<'py>, max_len: usize) -> PyResult<&'py PyAny> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut stream = inner.lock().await;
            let chunk = stream.read_chunk(max_len).await.map_err(err_to_py)?;
            let result = Python::with_gil(|py| match chunk {
                Some(chunk) => PyBytes::new(py, &chunk.bytes).into_py(py),
                None => py.None(),
            });
            Ok(result)
        })
    }

    fn read_exact<'py>(&self, py: Python<'py>, n: usize) -> PyResult<&'py PyAny> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut stream = inner.lock().await;
            let mut buf = vec![0u8; n];
            stream.read_exact(&mut buf).await.map_err(err_to_py)?;
            let result: PyObject = Python::with_gil(|py| PyBytes::new(py, &buf).into_py(py));
            Ok(result)
        })
    }

    /// Read all remaining data up to `max_size` bytes.
    fn read_to_end<'py>(&self, py: Python<'py>, max_size: usize) -> PyResult<&'py PyAny> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let mut stream = inner.lock().await;
            let data = stream.read_to_end(max_size).await.map_err(err_to_py)?;
            let result: PyObject = Python::with_gil(|py| PyBytes::new(py, &data).into_py(py));
            Ok(result)
        })
    }

    fn stop(&self, code: u64) -> PyResult<()> {
        let mut stream = self
            .inner
            .try_lock()
            .map_err(|_| err_to_py("recv stream is busy"))?;
        stream.stop(u64_to_varint(code)?).map_err(err_to_py)
    }
}

// ---------------------------------------------------------------------------
// Connection wrapper
// ---------------------------------------------------------------------------

#[pyclass]
pub struct IrohConnection {
    inner: Connection,
}

#[pymethods]
impl IrohConnection {
    /// Open a bidirectional QUIC stream, returning (send, recv).
    fn open_bi<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let (send, recv) = conn.open_bi().await.map_err(err_to_py)?;
            let send_stream = IrohSendStream {
                inner: Arc::new(Mutex::new(send)),
            };
            let recv_stream = IrohRecvStream {
                inner: Arc::new(Mutex::new(recv)),
            };
            Ok((send_stream, recv_stream))
        })
    }

    fn open_uni<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let send = conn.open_uni().await.map_err(err_to_py)?;
            Ok(IrohSendStream {
                inner: Arc::new(Mutex::new(send)),
            })
        })
    }

    /// Accept an incoming bidirectional stream from the peer.
    fn accept_bi<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let (send, recv) = conn.accept_bi().await.map_err(err_to_py)?;
            let send_stream = IrohSendStream {
                inner: Arc::new(Mutex::new(send)),
            };
            let recv_stream = IrohRecvStream {
                inner: Arc::new(Mutex::new(recv)),
            };
            Ok((send_stream, recv_stream))
        })
    }

    fn accept_uni<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let recv = conn.accept_uni().await.map_err(err_to_py)?;
            Ok(IrohRecvStream {
                inner: Arc::new(Mutex::new(recv)),
            })
        })
    }

    /// Send an unreliable datagram over this connection.
    fn send_datagram(&self, data: Vec<u8>) -> PyResult<()> {
        self.inner
            .send_datagram(Bytes::from(data))
            .map_err(err_to_py)
    }

    /// Read the next datagram received on this connection.
    fn read_datagram<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let data = conn.read_datagram().await.map_err(err_to_py)?;
            let result: PyObject = Python::with_gil(|py| PyBytes::new(py, &data).into_py(py));
            Ok(result)
        })
    }

    /// Return the remote endpoint's ID as a string.
    fn remote_id(&self) -> String {
        self.inner.remote_id().to_string()
    }

    fn close(&self, code: u64, reason: Vec<u8>) -> PyResult<()> {
        self.inner.close(u64_to_varint(code)?, &reason);
        Ok(())
    }

    fn closed<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let conn = self.inner.clone();
        future_into_py(py, async move {
            let closed = conn.closed().await;
            let kind = format!("{closed:?}");
            let (code, reason) = match &closed {
                ConnectionError::ApplicationClosed(app) => (
                    Some(u64::from(app.error_code.into_inner())),
                    Some(app.reason.to_vec()),
                ),
                _ => (None, Some(closed.to_string().into_bytes())),
            };
            let result = Python::with_gil(|py| -> PyResult<PyObject> {
                let d = PyDict::new(py);
                d.set_item("kind", kind)?;
                d.set_item("code", code)?;
                let reason_obj: Option<PyObject> = reason.map(|r| PyBytes::new(py, &r).into_py(py));
                d.set_item("reason", reason_obj)?;
                Ok(d.into_py(py))
            })?;
            Ok(result)
        })
    }
}

// ---------------------------------------------------------------------------
// NetClient — wraps an Endpoint for connecting / accepting
// ---------------------------------------------------------------------------

#[pyclass]
pub struct NetClient {
    pub(crate) endpoint: Endpoint,
}

#[pymethods]
impl NetClient {
    /// Connect to a remote node by its endpoint ID string and ALPN.
    fn connect<'py>(
        &self,
        py: Python<'py>,
        node_id: String,
        alpn: Vec<u8>,
    ) -> PyResult<&'py PyAny> {
        let endpoint = self.endpoint.clone();
        future_into_py(py, async move {
            let id: EndpointId = node_id.parse().map_err(err_to_py)?;
            let conn: Connection = endpoint.connect(id, &alpn).await.map_err(err_to_py)?;
            Ok(IrohConnection { inner: conn })
        })
    }

    fn connect_node_addr<'py>(
        &self,
        py: Python<'py>,
        addr: NodeAddr,
        alpn: Vec<u8>,
    ) -> PyResult<&'py PyAny> {
        let endpoint = self.endpoint.clone();
        future_into_py(py, async move {
            let addr = py_to_endpoint_addr(&addr)?;
            let conn: Connection = endpoint.connect(addr, &alpn).await.map_err(err_to_py)?;
            Ok(IrohConnection { inner: conn })
        })
    }

    /// Accept one incoming connection (only works on bare endpoints, not IrohNode).
    fn accept<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let endpoint = self.endpoint.clone();
        future_into_py(py, async move {
            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| err_to_py("endpoint closed, no incoming connection"))?;
            let conn = incoming
                .accept()
                .map_err(err_to_py)?
                .await
                .map_err(err_to_py)?;
            Ok(IrohConnection { inner: conn })
        })
    }

    /// Return this endpoint's ID as a hex string.
    fn endpoint_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Return the endpoint's address info (debug format).
    fn endpoint_addr(&self) -> String {
        format!("{:?}", self.endpoint.addr())
    }

    fn endpoint_addr_info(&self) -> NodeAddr {
        endpoint_addr_to_py(self.endpoint.addr())
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let endpoint = self.endpoint.clone();
        future_into_py(py, async move {
            endpoint.close().await;
            Ok(())
        })
    }

    fn closed<'py>(&self, py: Python<'py>) -> PyResult<&'py PyAny> {
        let endpoint = self.endpoint.clone();
        future_into_py(py, async move {
            endpoint.closed().await;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Factory functions
// ---------------------------------------------------------------------------

/// Get a NetClient backed by an IrohNode's endpoint (connect-only, Router owns accept).
#[pyfunction]
pub fn net_client(node: &IrohNode) -> NetClient {
    NetClient {
        endpoint: node.endpoint.clone(),
    }
}

/// Create a bare QUIC endpoint (no Router, no protocols) for custom QUIC usage.
/// Supports both connect and accept.
#[pyfunction]
pub fn create_endpoint<'py>(py: Python<'py>, alpn: Vec<u8>) -> PyResult<&'py PyAny> {
    future_into_py(py, async move {
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(vec![alpn])
            .bind()
            .await
            .map_err(err_to_py)?;
        endpoint.online().await;
        Ok(NetClient { endpoint })
    })
}

#[pyfunction]
pub fn create_endpoint_with_config<'py>(
    py: Python<'py>,
    config: EndpointConfig,
) -> PyResult<&'py PyAny> {
    future_into_py(py, async move {
        let relay_mode = relay_mode_from_config(&config)?;
        let endpoint = build_endpoint_config(config)?
            .bind()
            .await
            .map_err(err_to_py)?;
        if !matches!(relay_mode, RelayMode::Disabled) {
            endpoint.online().await;
        }
        Ok(NetClient { endpoint })
    })
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<NodeAddr>()?;
    m.add_class::<EndpointConfig>()?;
    m.add_class::<NetClient>()?;
    m.add_class::<IrohConnection>()?;
    m.add_class::<IrohSendStream>()?;
    m.add_class::<IrohRecvStream>()?;
    m.add_function(wrap_pyfunction!(net_client, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint_with_config, m)?)?;
    Ok(())
}
