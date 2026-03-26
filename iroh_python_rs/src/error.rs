use pyo3::{create_exception, exceptions::PyException, prelude::*};

// Base exception for all Iroh errors
create_exception!(iroh_python, IrohError, PyException);

// Specific error types
create_exception!(iroh_python, BlobNotFound, IrohError);
create_exception!(iroh_python, DocNotFound, IrohError);
create_exception!(iroh_python, ConnectionError, IrohError);
create_exception!(iroh_python, TicketError, IrohError);

/// Convert any Display error to Python exception
pub fn err_to_py(e: impl std::fmt::Display) -> PyErr {
    IrohError::new_err(e.to_string())
}

/// Convert anyhow::Error to Python exception
/// 
/// This is the central error conversion function. All Rust errors should
/// be mapped through this function before being returned to Python.
/// 
/// Future enhancement: Add downcasting to detect specific error types
/// and map them to the appropriate Python exception subclass.
pub fn anyhow_to_py(e: anyhow::Error) -> PyErr {
    // For now, convert all errors to the base IrohError
    // TODO: Add downcasting logic for specific error types:
    // - Check for blob-not-found errors -> BlobNotFound
    // - Check for doc-not-found errors -> DocNotFound
    // - Check for connection errors -> ConnectionError
    // - Check for ticket parsing errors -> TicketError
    IrohError::new_err(e.to_string())
}

/// Register all exception types with the Python module
pub fn register(py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add("IrohError", py.get_type::<IrohError>())?;
    m.add("BlobNotFound", py.get_type::<BlobNotFound>())?;
    m.add("DocNotFound", py.get_type::<DocNotFound>())?;
    m.add("ConnectionError", py.get_type::<ConnectionError>())?;
    m.add("TicketError", py.get_type::<TicketError>())?;
    Ok(())
}
