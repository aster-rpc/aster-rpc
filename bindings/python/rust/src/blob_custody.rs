//! Thin Python ownership/cancellation bridge for shared blob custody.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aster_transport_core::{CoreBlobCustody, CoreBlobCustodyError};
use pyo3::exceptions::{PyException, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;

use crate::blobs::{BlobFormatArg, FetchReport, FetchStrategy};
use crate::ids::HexArg;
use crate::{error::err_to_py, PyBytesResult};

pyo3::create_exception!(aster._aster, _BlobOperationCancelled, PyException);

pub(crate) fn error(error: anyhow::Error) -> PyErr {
    match error.downcast_ref::<CoreBlobCustodyError>() {
        Some(CoreBlobCustodyError::Cancelled) => {
            _BlobOperationCancelled::new_err(error.to_string())
        }
        Some(CoreBlobCustodyError::HashMismatch { .. }) => PyValueError::new_err(error.to_string()),
        _ => err_to_py(error),
    }
}

pub(crate) fn format(value: BlobFormatArg) -> String {
    match value.0 {
        aster::BlobFormat::Raw => "raw",
        aster::BlobFormat::HashSeq => "hash_seq",
    }
    .into()
}

#[pyclass(frozen, module = "aster._aster", name = "_BlobCancellation")]
pub(crate) struct BlobCancellation {
    flag: Arc<AtomicBool>,
}
#[pymethods]
impl BlobCancellation {
    fn request_cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

type Starter = Box<dyn for<'py> FnOnce(Python<'py>) -> PyResult<Py<PyAny>> + Send>;

#[pyclass(frozen, module = "aster._aster", name = "_PendingBlobOperation")]
struct PendingBlobOperation {
    starter: std::sync::Mutex<Option<Starter>>,
}
#[pymethods]
impl PendingBlobOperation {
    fn start(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let starter = self
            .starter
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("blob operation already started"))?;
        starter(py)
    }
}

/// Shield Python cancellation until native cleanup/commit has a terminal result.
pub(crate) fn operation<'py, T: for<'a> IntoPyObject<'a> + Send + 'static>(
    py: Python<'py>,
    flag: Arc<AtomicBool>,
    future: impl std::future::Future<Output = PyResult<T>> + Send + 'static,
) -> PyResult<Bound<'py, PyAny>> {
    // Delay native scheduling until the Python coroutine first runs. A task
    // cancelled before its first step drops this future/lease without effects.
    let starter: Starter = Box::new(move |py| Ok(future_into_py(py, future)?.unbind()));
    let pending = Py::new(
        py,
        PendingBlobOperation {
            starter: std::sync::Mutex::new(Some(starter)),
        },
    )?;
    let cancellation = Py::new(py, BlobCancellation { flag })?;
    py.import("aster.custody")?
        .getattr("_await_operation")?
        .call1((pending, cancellation))
}

/// In-memory GC custody for multiple Raw/HashSeq roots. close rejects new work;
/// aclose additionally waits for all admitted native work to release its guards.
#[pyclass(module = "aster._aster")]
pub struct BlobCustody {
    pub(crate) inner: CoreBlobCustody,
}
#[pymethods]
impl BlobCustody {
    fn close(&self) {
        self.inner.close();
    }
    fn aclose<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let scope = self.inner.clone();
        scope.close();
        future_into_py(py, async move {
            scope.close_and_wait().await;
            Ok(())
        })
    }
    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move { Ok(slf) })
    }
    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _ty: &Bound<'_, PyAny>,
        _value: &Bound<'_, PyAny>,
        _tb: &Bound<'_, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.aclose(py)
    }
    /// Pin before fetch, or add a root discovered during host verification.
    #[pyo3(signature = (hash_hex, *, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn protect<'py>(
        &self,
        py: Python<'py>,
        hash_hex: HexArg,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.inner.lease().map_err(error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        operation(py, flag, async move {
            lease
                .protect_cancellable(
                    hash_hex.0,
                    crate::blob_custody::format(format),
                    Some(cancel),
                )
                .await
                .map_err(error)
        })
    }
    /// Import under temporary custody without an anonymous persistent tag.
    #[pyo3(signature = (data, *, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn add_bytes<'py>(
        &self,
        py: Python<'py>,
        data: Vec<u8>,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.inner.lease().map_err(error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        operation(py, flag, async move {
            lease
                .add_bytes_cancellable(data, crate::blob_custody::format(format), Some(cancel))
                .await
                .map_err(error)
        })
    }
    /// Copy import; later source edits never alter store-owned bytes.
    #[pyo3(signature = (path, *, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn add_path<'py>(
        &self,
        py: Python<'py>,
        path: String,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.inner.lease().map_err(error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        operation(py, flag, async move {
            lease
                .add_path_cancellable(path, crate::blob_custody::format(format), Some(cancel))
                .await
                .map_err(error)
        })
    }
    /// Commit an exactly protected root/format under a persistent named tag.
    #[pyo3(signature = (hash_hex, tag_name, *, format=BlobFormatArg(aster::BlobFormat::Raw)))]
    fn retain<'py>(
        &self,
        py: Python<'py>,
        hash_hex: HexArg,
        tag_name: String,
        format: BlobFormatArg,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.inner.lease().map_err(error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        operation(py, flag, async move {
            lease
                .retain(
                    hash_hex.0,
                    crate::blob_custody::format(format),
                    tag_name,
                    Some(cancel),
                )
                .await
                .map_err(error)
        })
    }
    /// Read root bytes without interpreting HashSeq as a Collection.
    fn read<'py>(&self, py: Python<'py>, hash_hex: HexArg) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.inner.lease().map_err(error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        operation(py, flag, async move {
            lease
                .read_cancellable(hash_hex.0, Some(cancel))
                .await
                .map(PyBytesResult)
                .map_err(error)
        })
    }
    /// Fetch to store, retaining custody through native cancellation drain.
    #[pyo3(signature = (hash_hex, providers, *, format=BlobFormatArg(aster::BlobFormat::Raw), strategy=FetchStrategy::Whole))]
    fn download<'py>(
        &self,
        py: Python<'py>,
        hash_hex: HexArg,
        providers: Vec<HexArg>,
        format: BlobFormatArg,
        strategy: FetchStrategy,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.inner.lease().map_err(error)?;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        operation(py, flag, async move {
            let strategy = match strategy {
                FetchStrategy::Whole => aster_transport_core::CoreFetchStrategy::Whole,
                FetchStrategy::SplitChildren => {
                    aster_transport_core::CoreFetchStrategy::SplitChildren
                }
            };
            lease
                .download_cancellable(
                    hash_hex.0,
                    crate::blob_custody::format(format),
                    providers.into_iter().map(|p| p.0).collect(),
                    strategy,
                    Some(cancel),
                )
                .await
                .map(|report| FetchReport::from(aster::FetchReport::from(report)))
                .map_err(error)
        })
    }
}
impl Drop for BlobCustody {
    fn drop(&mut self) {
        self.inner.close();
    }
}

pub(crate) fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<BlobCustody>()?;
    m.add_class::<BlobCancellation>()?;
    m.add_class::<PendingBlobOperation>()?;
    m.add(
        "_BlobOperationCancelled",
        py.get_type::<_BlobOperationCancelled>(),
    )?;
    Ok(())
}
