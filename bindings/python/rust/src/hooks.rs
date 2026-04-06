//! Hooks module - Python wrappers for Phase 1b endpoint hooks.
//!
//! This module provides Python access to the hook system exposed through
//! aster_transport_core's CoreHookReceiver.
//!
//! Phase 1b: Hooks are enabled via EndpointConfig.enable_hooks = true.
//! The hook receiver is consumed from CoreNetClient via take_hook_receiver().
//!
//! Hooks work as follows:
//! 1. Create an endpoint with EndpointConfig(enable_hooks=True)
//! 2. Call net_client.has_hooks() to verify hooks are enabled
//! 3. Set up callbacks for before_connect and after_connect events
//! 4. The hook receiver drains events and dispatches to Python callbacks

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use std::sync::Arc;
use tokio::sync::Mutex;

use aster_transport_core::{CoreAfterHandshakeDecision, CoreHookReceiver};

/// Hook event types
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct HookConnectInfo {
    #[pyo3(get)]
    pub remote_endpoint_id: String,
    #[pyo3(get)]
    pub alpn: Vec<u8>,
}

#[pymethods]
impl HookConnectInfo {
    #[new]
    #[pyo3(signature = (remote_endpoint_id="".to_string(), alpn=vec![]))]
    fn new(remote_endpoint_id: String, alpn: Vec<u8>) -> Self {
        Self {
            remote_endpoint_id,
            alpn,
        }
    }
}

/// Hook handshake event types
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct HookHandshakeInfo {
    #[pyo3(get)]
    pub remote_endpoint_id: String,
    #[pyo3(get)]
    pub alpn: Vec<u8>,
    #[pyo3(get)]
    pub is_alive: bool,
}

#[pymethods]
impl HookHandshakeInfo {
    #[new]
    #[pyo3(signature = (remote_endpoint_id="".to_string(), alpn=vec![], is_alive=false))]
    fn new(remote_endpoint_id: String, alpn: Vec<u8>, is_alive: bool) -> Self {
        Self {
            remote_endpoint_id,
            alpn,
            is_alive,
        }
    }
}

/// Hook decision types
#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub struct HookDecision {
    /// Whether this decision allows the connection (True) or denies it (False).
    #[pyo3(get)]
    pub is_allowed: bool,
    #[pyo3(get)]
    pub error_code: Option<u32>,
    #[pyo3(get)]
    pub reason: Option<Vec<u8>>,
}

#[pymethods]
impl HookDecision {
    #[new]
    fn new() -> Self {
        Self {
            is_allowed: true,
            error_code: None,
            reason: None,
        }
    }

    /// Whether this decision allows the connection.
    /// Alias for is_allowed, kept for backwards compatibility with tests.
    #[getter]
    fn allow(&self) -> bool {
        self.is_allowed
    }

    /// Create an Allow decision
    #[staticmethod]
    fn create_allow() -> Self {
        Self {
            is_allowed: true,
            error_code: None,
            reason: None,
        }
    }

    /// Create a Deny decision
    #[staticmethod]
    fn create_deny(error_code: u32, reason: Vec<u8>) -> Self {
        Self {
            is_allowed: false,
            error_code: Some(error_code),
            reason: Some(reason),
        }
    }
}

impl Default for HookDecision {
    fn default() -> Self {
        Self::create_allow()
    }
}

/// HookReceiver wrapper for consuming hook events in Python.
///
/// This is a simplified wrapper - in practice, the hook receiver
/// is consumed internally by the core layer when enable_hooks=True.
/// Python callbacks can be registered at the endpoint level.
///
/// Note: The actual hook implementation in aster_transport_core uses
/// channel-based communication. For Python, the recommended pattern is:
/// - Use EndpointConfig(enable_hooks=True) when creating an endpoint
/// - Pass Python callback functions when creating the endpoint
/// - The callbacks will be invoked when hook events occur
///
/// This module provides type definitions for the hook system.
/// The actual hook dispatch is handled by the core layer.
#[pyclass]
pub struct HookReceiver {
    _inner: Arc<()>, // Placeholder - actual receiver is in core
}

impl HookReceiver {
    pub fn new() -> Self {
        Self {
            _inner: Arc::new(()),
        }
    }
}

impl Default for HookReceiver {
    fn default() -> Self {
        Self::new()
    }
}

#[pymethods]
impl HookReceiver {
    /// Check if there are pending hook events to process.
    fn has_pending(&self) -> bool {
        // This is a stub - actual implementation would check the channel
        false
    }
}

/// Hook registration for setting up callbacks.
///
/// Usage:
/// ```python
/// def my_before_connect(info):
///     print(f"Connecting to {info.remote_endpoint_id}")
///     return HookDecision.Allow()
///     # or HookDecision.Deny(error_code=1, reason=b"not allowed")
///
/// def my_after_connect(info):
///     print(f"Connected to {info.remote_endpoint_id}, alive={info.is_alive}")
///     return HookDecision.Allow()
///
/// registration = endpoint.set_hooks(
///     before_connect=my_before_connect,
///     after_connect=my_after_connect,
/// )
/// ```
#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub struct HookRegistration {
    #[pyo3(get)]
    pub has_before_connect: bool,
    #[pyo3(get)]
    pub has_after_connect: bool,
}

/// HookManager for registering and managing hooks.
///
/// Note: This is a simplified interface. The actual hook system
/// in aster_transport_core uses channel-based communication with
/// configurable timeouts. Hooks are registered at endpoint creation
/// time via EndpointConfig(enable_hooks=True).
#[pyclass]
pub struct HookManager {
    _inner: Arc<Mutex<()>>,
}

impl HookManager {
    pub fn new() -> Self {
        Self {
            _inner: Arc::new(Mutex::new(())),
        }
    }
}

impl Default for HookManager {
    fn default() -> Self {
        Self::new()
    }
}

#[pymethods]
impl HookManager {
    /// Create a new hook manager with the given callbacks.
    ///
    /// Args:
    ///     before_connect: Optional callback(hook_info) -> HookDecision
    ///     after_connect: Optional callback(hook_info) -> HookDecision
    ///
    /// Returns:
    ///     HookRegistration with information about registered hooks
    #[new]
    #[pyo3(signature = (_before_connect=None, _after_connect=None))]
    fn new_with_callbacks(
        _before_connect: Option<Py<PyAny>>,
        _after_connect: Option<Py<PyAny>>,
    ) -> Self {
        // Note: In the actual implementation, callbacks would be stored
        // and dispatched when hook events arrive from the core layer.
        // For now, this is a placeholder structure.
        Self::new()
    }

    /// Update the before_connect callback.
    fn set_before_connect(&mut self, _callback: Option<Py<PyAny>>) {
        // Placeholder - actual implementation would update the stored callback
    }

    /// Update the after_connect callback.
    fn set_after_connect(&mut self, _callback: Option<Py<PyAny>>) {
        // Placeholder - actual implementation would update the stored callback
    }
}

// =============================================================================
// Real NodeHookReceiver — drains CoreHookReceiver channels.
// =============================================================================
//
// The receiver above is a stub. This is the real thing, consumed by
// IrohNode.take_hook_receiver() when built with enable_hooks=true.
//
// Design:
//   * A background tokio task auto-accepts every `before_connect` request
//     (the peer ID isn't authenticated yet; Gate 0 runs at after_handshake
//     where we have the verified EndpointId).
//   * `NodeHookReceiver.recv()` surfaces each `after_handshake` event as
//     `(HookHandshakeInfo, NodeHookDecisionSender)` so Python can apply
//     its allowlist decision asynchronously.
//   * `NodeHookDecisionSender.send(HookDecision)` consumes the underlying
//     `oneshot::Sender<CoreAfterHandshakeDecision>`.

/// Single-use sender for an after-handshake decision. Wraps a oneshot
/// channel that the corresponding `CoreHooksAdapter::after_handshake` is
/// waiting on.
#[pyclass]
pub struct NodeHookDecisionSender {
    tx: Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<CoreAfterHandshakeDecision>>>>,
}

#[pymethods]
impl NodeHookDecisionSender {
    /// Send a decision (Allow or Deny). May only be called once per sender.
    fn send<'py>(
        &self,
        py: Python<'py>,
        decision: Py<HookDecision>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let tx = self.tx.clone();
        // Extract fields now (while we still have the GIL) so the async body
        // doesn't need to re-acquire it.
        let (is_allowed, error_code, reason) = {
            let bound = decision.bind(py).borrow();
            (bound.is_allowed, bound.error_code, bound.reason.clone())
        };
        future_into_py(py, async move {
            let sender = tx
                .lock()
                .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("hook sender poisoned"))?
                .take();
            let Some(sender) = sender else {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "hook decision already sent",
                ));
            };
            let core_decision = if is_allowed {
                CoreAfterHandshakeDecision::Accept
            } else {
                CoreAfterHandshakeDecision::Reject {
                    error_code: error_code.unwrap_or(403),
                    reason: reason.unwrap_or_else(|| b"denied".to_vec()),
                }
            };
            // If recv side dropped we swallow — the adapter defaults to Accept.
            let _ = sender.send(core_decision);
            Ok(())
        })
    }

    /// Make the class callable as `await sender(decision)` for symmetry with
    /// the existing stub API.
    fn __call__<'py>(
        &self,
        py: Python<'py>,
        decision: Py<HookDecision>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.send(py, decision)
    }
}

type AfterHandshakeRx = Arc<
    Mutex<
        tokio::sync::mpsc::Receiver<(
            aster_transport_core::CoreHookHandshakeInfo,
            tokio::sync::oneshot::Sender<CoreAfterHandshakeDecision>,
        )>,
    >,
>;

/// Node-level hook receiver. Obtained via `IrohNode.take_hook_receiver()`.
/// Call `recv()` in a loop to drain after-handshake events and make Gate 0
/// decisions; `before_connect` is auto-accepted in the background task.
#[pyclass]
pub struct NodeHookReceiver {
    /// Only the after_handshake side is exposed to Python. The
    /// before_connect side is drained by `_before_connect_task`.
    after_rx: AfterHandshakeRx,
    /// Abort handle for the background before_connect drainer.
    _before_connect_task: Arc<tokio::task::AbortHandle>,
}

impl NodeHookReceiver {
    pub(crate) fn from_core(core: CoreHookReceiver) -> Self {
        let CoreHookReceiver {
            mut before_connect_rx,
            after_handshake_rx,
        } = core;

        // Background task: always allow the before_connect step. The
        // peer's EndpointId isn't authenticated here; Gate 0 gates at
        // after_handshake where we have the verified remote id.
        let task = tokio::spawn(async move {
            while let Some((_info, reply_tx)) = before_connect_rx.recv().await {
                let _ = reply_tx.send(true);
            }
        });
        let abort = task.abort_handle();

        Self {
            after_rx: Arc::new(Mutex::new(after_handshake_rx)),
            _before_connect_task: Arc::new(abort),
        }
    }
}

#[pymethods]
impl NodeHookReceiver {
    /// Await the next after-handshake event. Returns
    /// `(HookHandshakeInfo, NodeHookDecisionSender)`, or `None` when the
    /// underlying channel closes (node shut down).
    fn recv<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.after_rx.clone();
        future_into_py(py, async move {
            let mut guard = rx.lock().await;
            let Some((info, sender)) = guard.recv().await else {
                return Python::attach(|py| Ok::<Py<PyAny>, PyErr>(py.None()));
            };
            let info_py = HookHandshakeInfo {
                remote_endpoint_id: info.remote_endpoint_id,
                alpn: info.alpn,
                is_alive: info.is_alive,
            };
            let sender_py = NodeHookDecisionSender {
                tx: Arc::new(std::sync::Mutex::new(Some(sender))),
            };
            Python::attach(|py| {
                let info_obj: Py<PyAny> = Py::new(py, info_py)?.into_any();
                let sender_obj: Py<PyAny> = Py::new(py, sender_py)?.into_any();
                let tup = pyo3::types::PyTuple::new(py, &[info_obj, sender_obj])?;
                Ok::<Py<PyAny>, PyErr>(tup.unbind().into())
            })
        })
    }
}

/// Register the hooks types with the Python module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<HookConnectInfo>()?;
    m.add_class::<HookHandshakeInfo>()?;
    m.add_class::<HookDecision>()?;
    m.add_class::<HookReceiver>()?;
    m.add_class::<HookRegistration>()?;
    m.add_class::<HookManager>()?;
    m.add_class::<NodeHookReceiver>()?;
    m.add_class::<NodeHookDecisionSender>()?;
    Ok(())
}
