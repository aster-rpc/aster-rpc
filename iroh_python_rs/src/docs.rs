use pyo3::prelude::*;
use iroh_docs::protocol::Docs;

use crate::node::IrohNode;

#[pyclass]
pub struct DocsClient {
    pub(crate) inner: Docs,
}

#[pyfunction]
pub fn docs_client(node: &IrohNode) -> DocsClient {
    DocsClient {
        inner: node.docs.clone(),
    }
}

pub fn register(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<DocsClient>()?;
    m.add_function(wrap_pyfunction!(docs_client, m)?)?;
    Ok(())
}