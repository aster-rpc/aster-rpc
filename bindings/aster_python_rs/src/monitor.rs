//! Monitor module - Python wrappers for Phase 1b monitoring surfaces.
//!
//! This module provides Python access to connection monitoring and remote-info
//! APIs exposed through aster_transport_core's CoreMonitor.
//!
//! Phase 1b: Remote-info and monitoring APIs are now available via NetClient.

use pyo3::prelude::*;

// Note: The actual monitoring implementation is in aster_transport_core::CoreMonitor.
// Python access is provided through NetClient methods:
// - NetClient.remote_info(node_id) -> RemoteInfo | None
// - NetClient.remote_info_list() -> Vec<RemoteInfo>
// - NetClient.has_monitoring() -> bool
//
// And connection info through IrohConnection:
// - IrohConnection.connection_info() -> ConnectionInfo
//
// This module is a placeholder for any additional Python-specific monitoring
// utilities that may be needed in the future.

/// Register the monitoring-related types with the Python module.
///
/// Note: ConnectionInfo and RemoteInfo are registered in net.rs as they
/// are used directly by NetClient and IrohConnection.
pub fn register(_py: Python<'_>, _m: &Bound<'_, PyModule>) -> PyResult<()> {
    // ConnectionInfo and RemoteInfo are registered in net.rs::register()
    Ok(())
}
