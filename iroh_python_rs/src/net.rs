use std::sync::Arc;
use tokio::sync::Mutex;

use bytes::Bytes;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_asyncio::tokio::future_into_py;

use iroh::endpoint::{presets, Connection, Endpoint};
use iroh::EndpointId;

use crate::error::err_to_py;
use crate::node::IrohNode;

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

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<NetClient>()?;
    m.add_class::<IrohConnection>()?;
    m.add_class::<IrohSendStream>()?;
    m.add_class::<IrohRecvStream>()?;
    m.add_function(wrap_pyfunction!(net_client, m)?)?;
    m.add_function(wrap_pyfunction!(create_endpoint, m)?)?;
    Ok(())
}
