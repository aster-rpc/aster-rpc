//! Error module - Exception types for Python bindings.
//!
//! Phase 2: Updated to work with aster_transport_core types.

use pyo3::{create_exception, exceptions::PyException, prelude::*};

// Base exception for all Iroh errors
create_exception!(aster, IrohError, PyException);

// Specific error types
create_exception!(aster, BlobNotFound, IrohError);
create_exception!(aster, DocNotFound, IrohError);
create_exception!(aster, ConnectionError, IrohError);
create_exception!(aster, TicketError, IrohError);

/// Convert any Display error to Python exception
pub fn err_to_py(e: impl std::fmt::Display) -> PyErr {
    IrohError::new_err(e.to_string())
}

/// Convert an `aster` facade error. Invalid arguments (bad key file, bad
/// config value, mismatched identities) raise `ValueError`; the recognisable
/// transport failures map to the matching `IrohError` subclass.
pub fn aster_err_to_py(e: aster::Error) -> PyErr {
    match e {
        aster::Error::InvalidArgument(msg) => pyo3::exceptions::PyValueError::new_err(msg),
        aster::Error::BlobNotFound(msg) => BlobNotFound::new_err(msg),
        aster::Error::DocNotFound(msg) => DocNotFound::new_err(msg),
        aster::Error::Connection(msg) => ConnectionError::new_err(msg),
        aster::Error::Ticket(msg) => TicketError::new_err(msg),
        other => IrohError::new_err(other.to_string()),
    }
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
