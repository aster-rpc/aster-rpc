//! Owned Python HTTP/WT serving through the shared dispatcher and auth upcalls.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use aster::binding_support::upcall::auth::{ForeignAuthenticator, ForeignAuthenticatorOptions};
use aster::rpc::{Authenticator, Dispatcher};
use aster_transport_salvo::{HttpConfig, OwnedListener, TlsMaterial};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::Mutex;

#[derive(Default)]
pub(crate) struct HttpChildren {
    closed: AtomicBool,
    children: Mutex<Vec<Weak<ListenerInner>>>,
}

impl HttpChildren {
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) async fn stop_all(&self) -> PyResult<()> {
        self.close();
        let mut children = self.children.lock().await;
        let mut error = None;
        for child in children.iter().filter_map(Weak::upgrade) {
            if let Err(e) = child.stop(Duration::ZERO).await {
                error.get_or_insert(e);
            }
        }
        // Preserve owners on error so an awaited close can retry cleanup.
        if let Some(error) = error {
            return Err(error);
        }
        children.clear();
        Ok(())
    }

    pub(crate) fn force_cleanup(self: &Arc<Self>) {
        self.close();
        let owner = self.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let _ = owner.stop_all().await;
        });
    }
}

struct ListenerInner {
    owned: Mutex<Option<OwnedListener>>,
}

impl ListenerInner {
    async fn stop(&self, grace: Duration) -> PyResult<()> {
        let mut owned = self.owned.lock().await;
        if let Some(listener) = owned.as_mut() {
            listener
                .stop(grace)
                .await
                .map_err(PyRuntimeError::new_err)?;
        }
        owned.take();
        Ok(())
    }
}

pub(crate) fn authenticator(
    handler: &Bound<'_, PyAny>,
    max_pending: usize,
    max_payload_bytes: usize,
) -> PyResult<Arc<dyn Authenticator>> {
    let upcalls = crate::upcall::bind(handler, "rpc_authenticate")?;
    ForeignAuthenticator::with_options(
        upcalls,
        ForeignAuthenticatorOptions {
            max_pending,
            max_payload_bytes,
        },
    )
    .map(|auth| Arc::new(auth) as Arc<dyn Authenticator>)
    .map_err(|e| PyValueError::new_err(e.to_string()))
}

pub(crate) enum ListenerPolicy {
    Authenticated(Arc<dyn Authenticator>),
    Public,
}

pub(crate) async fn start(
    owner: Arc<HttpChildren>,
    dispatcher: Dispatcher,
    policy: ListenerPolicy,
    bind_addr: String,
    cert: Vec<u8>,
    key: Vec<u8>,
    webtransport: bool,
) -> PyResult<HttpListener> {
    // Close waits for startup and then joins every published child. A cancelled
    // start drops its local OwnedListener, which schedules forced native cleanup.
    let mut children = owner.children.lock().await;
    if owner.closed.load(Ordering::Acquire) {
        return Err(PyRuntimeError::new_err("server is closed"));
    }
    let config = HttpConfig::new(bind_addr, TlsMaterial::pem(cert, key)).webtransport(webtransport);
    let mut listener = match policy {
        ListenerPolicy::Authenticated(auth) => config.start_authenticated(dispatcher, auth).await,
        ListenerPolicy::Public => config.start_public(dispatcher).await,
    }
    .map_err(PyValueError::new_err)?;
    if owner.closed.load(Ordering::Acquire) {
        listener
            .stop(Duration::ZERO)
            .await
            .map_err(PyRuntimeError::new_err)?;
        return Err(PyRuntimeError::new_err("server is closed"));
    }
    let tcp_addr = listener.tcp_addr().to_string();
    let quic_addr = listener.quic_addr().map(|addr| addr.to_string());
    let inner = Arc::new(ListenerInner {
        owned: Mutex::new(Some(listener)),
    });
    children.retain(|child| child.strong_count() > 0);
    children.push(Arc::downgrade(&inner));
    Ok(HttpListener {
        tcp_addr,
        quic_addr,
        inner,
    })
}

/// Native owner; prefer aster.http.HTTPListener.
#[pyclass(frozen, module = "aster._aster", name = "_HTTPListener")]
pub(crate) struct HttpListener {
    #[pyo3(get)]
    tcp_addr: String,
    #[pyo3(get)]
    quic_addr: Option<String>,
    inner: Arc<ListenerInner>,
}

#[pymethods]
impl HttpListener {
    #[pyo3(signature = (grace=0.0))]
    fn stop<'py>(&self, py: Python<'py>, grace: f64) -> PyResult<Bound<'py, PyAny>> {
        if !grace.is_finite() || grace < 0.0 {
            return Err(PyValueError::new_err(
                "grace must be finite and nonnegative",
            ));
        }
        let grace = Duration::try_from_secs_f64(grace)
            .map_err(|_| PyValueError::new_err("grace exceeds the supported duration"))?;
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.stop(grace).await })
    }
}

impl Drop for HttpListener {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let _ = inner.stop(Duration::ZERO).await;
        });
    }
}

fn secret(bytes: Option<Vec<u8>>) -> PyResult<Option<[u8; 32]>> {
    bytes
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| PyValueError::new_err("node_secret must contain exactly 32 bytes"))
        })
        .transpose()
}

type CertificateTuple<'py> = (
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
    Bound<'py, PyBytes>,
);

fn certificate_tuple<'py>(
    py: Python<'py>,
    cert: aster_transport_salvo::GeneratedCert,
) -> CertificateTuple<'py> {
    (
        PyBytes::new(py, &cert.cert_pem),
        PyBytes::new(py, &cert.key_pem),
        PyBytes::new(py, &cert.sha256),
    )
}

#[pyfunction]
#[pyo3(signature = (sans, node_secret=None))]
fn _generate_self_signed_certificate<'py>(
    py: Python<'py>,
    sans: Vec<String>,
    node_secret: Option<Vec<u8>>,
) -> PyResult<CertificateTuple<'py>> {
    let secret = secret(node_secret)?;
    let binding = secret
        .as_ref()
        .map(aster_transport_salvo::NodeBinding::Signed);
    let cert = aster_transport_salvo::generate_self_signed(binding, &sans)
        .map_err(PyValueError::new_err)?;
    Ok(certificate_tuple(py, cert))
}

#[pyfunction]
#[pyo3(signature = (subject, sans, node_secret=None))]
fn _generate_webtransport_certificate<'py>(
    py: Python<'py>,
    subject: String,
    sans: Vec<String>,
    node_secret: Option<Vec<u8>>,
) -> PyResult<CertificateTuple<'py>> {
    let secret = secret(node_secret)?;
    let binding = secret
        .as_ref()
        .map(aster_transport_salvo::NodeBinding::Signed);
    let cert = aster_transport_salvo::generate_webtransport_cert(&subject, binding, &sans)
        .map_err(PyValueError::new_err)?;
    Ok(certificate_tuple(py, cert))
}

#[pyfunction]
fn _verify_certificate_binding(certificate_pem: Vec<u8>, expected_node_id: String) -> PyResult<()> {
    aster_transport_salvo::verify_cert_binding(&certificate_pem, &expected_node_id)
        .map_err(PyValueError::new_err)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<HttpListener>()?;
    m.add_function(wrap_pyfunction!(_generate_self_signed_certificate, m)?)?;
    m.add_function(wrap_pyfunction!(_generate_webtransport_certificate, m)?)?;
    m.add_function(wrap_pyfunction!(_verify_certificate_binding, m)?)?;
    Ok(())
}
