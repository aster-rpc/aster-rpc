//! Error module - Exception types for Python bindings.
//!
//! Phase 2: Updated to work with aster_transport_core types.

use pyo3::{create_exception, exceptions::PyException, prelude::*};

// Base exception for all Iroh errors
create_exception!(aster_python, IrohError, PyException);

// Specific error types
create_exception!(aster_python, BlobNotFound, IrohError);
create_exception!(aster_python, DocNotFound, IrohError);
create_exception!(aster_python, ConnectionError, IrohError);
create_exception!(aster_python, TicketError, IrohError);

/// Convert any Display error to Python exception
pub fn err_to_py(e: impl std::fmt::Display) -> PyErr {
    IrohError::new_err(e.to_string())
}

/// Register all exception types with the Python module
pub fn register(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("IrohError", py.get_type::<IrohError>())?;
    m.add("BlobNotFound", py.get_type::<BlobNotFound>())?;
    m.add("DocNotFound", py.get_type::<DocNotFound>())?;
    m.add("ConnectionError", py.get_type::<ConnectionError>())?;
    m.add("TicketError", py.get_type::<TicketError>())?;
    Ok(())
}
