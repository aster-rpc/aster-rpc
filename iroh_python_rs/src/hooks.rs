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
use std::sync::Arc;
use tokio::sync::Mutex;

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

/// Register the hooks types with the Python module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<HookConnectInfo>()?;
    m.add_class::<HookHandshakeInfo>()?;
    m.add_class::<HookDecision>()?;
    m.add_class::<HookReceiver>()?;
    m.add_class::<HookRegistration>()?;
    m.add_class::<HookManager>()?;
    Ok(())
}
