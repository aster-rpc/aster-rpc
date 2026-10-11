//! Thin Python owners for the shared raw WebTransport implementation.
use crate::PyBytesResult;
use aster_transport_salvo::raw_webtransport::{
    RawWtConfig, RawWtListener, RawWtSession, RawWtStream,
};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use std::time::Duration;

fn err(e: String) -> PyErr {
    PyRuntimeError::new_err(e)
}
fn json<T: serde::Serialize>(v: &T) -> PyResult<String> {
    serde_json::to_string(v).map_err(|e| err(e.to_string()))
}

#[pyclass(frozen, module = "aster._aster", name = "_RawWTListener")]
struct Listener {
    inner: RawWtListener,
}
#[pymethods]
impl Listener {
    #[getter]
    fn tcp_addr(&self) -> String {
        self.inner.tcp_addr().to_string()
    }
    #[getter]
    fn quic_addr(&self) -> String {
        self.inner.quic_addr().to_string()
    }
    fn accept<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(Session {
                inner: inner.accept().await.map_err(err)?,
            })
        })
    }
    fn stop<'py>(&self, py: Python<'py>, grace: f64) -> PyResult<Bound<'py, PyAny>> {
        if !grace.is_finite() || !(0.0..=4294967.295).contains(&grace) {
            return Err(PyValueError::new_err(
                "grace must be between 0 and 4294967.295 seconds",
            ));
        }
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .stop(Duration::from_secs_f64(grace))
                .await
                .map_err(err)
        })
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.inner.close();
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let _ = inner.stop(Duration::ZERO).await;
        });
    }
}

#[pyfunction]
#[pyo3(signature=(bind_addr, certificate_pem, private_key_pem, path, max_sessions=64, max_streams=128, max_datagrams=128, max_datagram_bytes=65536, max_read_bytes=1048576, authenticator=None))]
#[allow(clippy::too_many_arguments)]
fn _raw_wt_start<'py>(
    py: Python<'py>,
    bind_addr: String,
    certificate_pem: Vec<u8>,
    private_key_pem: Vec<u8>,
    path: String,
    max_sessions: usize,
    max_streams: usize,
    max_datagrams: usize,
    max_datagram_bytes: usize,
    max_read_bytes: usize,
    authenticator: Option<&Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    let auth = authenticator
        .map(|a| crate::http::authenticator(a, 256, 65536))
        .transpose()?;
    let config = RawWtConfig {
        bind_addr,
        certificate_pem,
        private_key_pem,
        path,
        max_sessions,
        max_streams,
        max_datagrams,
        max_datagram_bytes,
        max_read_bytes,
    };
    future_into_py(py, async move {
        let inner = match auth {
            Some(auth) => RawWtListener::start_authenticated(config, auth).await,
            None => RawWtListener::start_public(config).await,
        }
        .map_err(err)?;
        Ok(Listener { inner })
    })
}

#[pyclass(frozen, module = "aster._aster", name = "_RawWTSession")]
struct Session {
    inner: RawWtSession,
}
#[pymethods]
impl Session {
    fn info(&self) -> PyResult<String> {
        json(&self.inner.info())
    }
    fn stats(&self) -> PyResult<String> {
        json(&self.inner.stats())
    }
    fn max_datagram_size(&self) -> Option<usize> {
        self.inner.max_datagram_size()
    }
    fn send_datagram(&self, data: Vec<u8>) -> PyResult<()> {
        self.inner.send_datagram(&data).map_err(err)
    }
    fn receive_datagram<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .receive_datagram()
                .await
                .map(PyBytesResult)
                .map_err(err)
        })
    }
    fn accept_bi<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(Stream {
                inner: inner.accept_bi().await.map_err(err)?,
            })
        })
    }
    fn accept_uni<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(Stream {
                inner: inner.accept_uni().await.map_err(err)?,
            })
        })
    }
    fn open_bi<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(Stream {
                inner: inner.open_bi().await.map_err(err)?,
            })
        })
    }
    fn open_uni<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            Ok(Stream {
                inner: inner.open_uni().await.map_err(err)?,
            })
        })
    }
    fn close(&self, code: u32, reason: Vec<u8>) -> PyResult<()> {
        self.inner.close(code, &reason).map_err(err)
    }
    fn closed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move { json(&inner.closed().await) })
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.inner.close(0, b"Python session released");
    }
}

#[pyclass(frozen, module = "aster._aster", name = "_RawWTStream")]
struct Stream {
    inner: RawWtStream,
}
#[pymethods]
impl Stream {
    fn info(&self) -> PyResult<String> {
        json(&self.inner.info())
    }
    fn read<'py>(&self, py: Python<'py>, maximum: usize) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner.read(maximum).await.map(PyBytesResult).map_err(err)
        })
    }
    fn write<'py>(&self, py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, PyAny>> {
        if data.len() > self.inner.max_write_size() {
            return Err(err("write exceeds configured byte limit".to_string()));
        }
        let data = data.to_vec();
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.write(&data).await.map_err(err) })
    }
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.finish().await.map_err(err) })
    }
    fn reset<'py>(&self, py: Python<'py>, code: u32) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.reset(code).await.map_err(err) })
    }
    fn stop<'py>(&self, py: Python<'py>, code: u32) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.stop(code).await.map_err(err) })
    }
    fn set_priority<'py>(&self, py: Python<'py>, priority: i32) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner.set_priority(priority).await.map_err(err)
        })
    }
    fn priority<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.priority().await.map_err(err) })
    }
    fn transport_acked<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(
            py,
            async move { inner.transport_acked().await.map_err(err) },
        )
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            if inner.info().has_send {
                let _ = inner.reset(0).await;
            }
            if inner.info().has_receive {
                let _ = inner.stop(0).await;
            }
        });
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Listener>()?;
    m.add_class::<Session>()?;
    m.add_class::<Stream>()?;
    m.add_function(wrap_pyfunction!(_raw_wt_start, m)?)?;
    Ok(())
}
