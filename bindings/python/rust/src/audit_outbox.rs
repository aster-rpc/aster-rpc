//! Async value conversion only; durability and lifecycle live in `aster`.
use aster::audit_outbox::{
    AuditError, AuditEvent, AuditOutbox, Destination, OutboxConfig, MAX_EVENT_BYTES,
};
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use std::time::Duration;

fn error(kind: &str, message: String) -> PyErr {
    Python::attach(|py| {
        let result = py
            .import("aster.audit_outbox")
            .and_then(|m| m.getattr("AuditOutboxError"))
            .and_then(|cls| cls.call1((kind, message)));
        match result {
            Ok(e) => PyErr::from_value(e),
            Err(e) => e,
        }
    })
}

fn native_error(e: AuditError) -> PyErr {
    let kind = serde_json::to_value(e.kind()).expect("error kind serializes");
    error(kind.as_str().expect("error kind is string"), e.message)
}

fn invalid(_: impl std::fmt::Display) -> PyErr {
    error("invalid_event", "invalid audit argument".into())
}

#[pyclass(name = "NativeAuditOutbox", module = "aster._aster")]
struct NativeAuditOutbox {
    inner: AuditOutbox,
}

#[pymethods]
impl NativeAuditOutbox {
    fn clone_handle(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    #[staticmethod]
    fn open<'py>(py: Python<'py>, config_json: String) -> PyResult<Bound<'py, PyAny>> {
        crate::ensure_tokio_runtime();
        future_into_py(py, async move {
            if config_json.len() > 32 * 1024 {
                return Err(error(
                    "invalid_config",
                    "audit configuration exceeds limit".into(),
                ));
            }
            let config: OutboxConfig = serde_json::from_str(&config_json)
                .map_err(|_| error("invalid_config", "invalid audit configuration".into()))?;
            Ok(Self {
                inner: AuditOutbox::open(config).await.map_err(native_error)?,
            })
        })
    }

    fn submit<'py>(&self, py: Python<'py>, event_json: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            if event_json.len() > MAX_EVENT_BYTES + 1024 {
                return Err(error("event_too_large", "audit event exceeds limit".into()));
            }
            let event: AuditEvent = serde_json::from_str(&event_json).map_err(invalid)?;
            Ok(inner.submit(event).await.map_err(native_error)?.to_string())
        })
    }

    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            serde_json::to_string(&inner.status().await.map_err(native_error)?).map_err(invalid)
        })
    }

    fn flush<'py>(&self, py: Python<'py>, timeout: f64) -> PyResult<Bound<'py, PyAny>> {
        let timeout = Duration::try_from_secs_f64(timeout).map_err(invalid)?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner.flush(timeout).await.map_err(native_error)
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move { inner.close().await.map_err(native_error) })
    }

    fn reconfigure<'py>(
        &self,
        py: Python<'py>,
        destination_json: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            if destination_json.len() > 32 * 1024 {
                return Err(error(
                    "invalid_config",
                    "audit destination exceeds limit".into(),
                ));
            }
            let destination: Destination = serde_json::from_str(&destination_json)
                .map_err(|_| error("invalid_config", "invalid audit destination".into()))?;
            inner.reconfigure(destination).await.map_err(native_error)
        })
    }

    fn quarantine<'py>(
        &self,
        py: Python<'py>,
        event_id: String,
        reason: String,
        operator_reference: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner
                .quarantine(event_id, reason, operator_reference)
                .await
                .map_err(native_error)
        })
    }

    fn export<'py>(&self, py: Python<'py>, event_id: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            serde_json::to_string(&inner.export(event_id).await.map_err(native_error)?)
                .map_err(invalid)
        })
    }

    fn replay<'py>(&self, py: Python<'py>, event_id: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            inner.replay(event_id).await.map_err(native_error)
        })
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeAuditOutbox>()
}
