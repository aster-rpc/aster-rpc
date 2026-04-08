//! AsterTicket — PyO3 wrapper for compact ticket encoding.

use pyo3::prelude::*;

use aster_transport_core::ticket::{AsterTicket as CoreTicket, TicketCredential};

use crate::PyBytesResult;

/// Compact Aster ticket: endpoint address + optional credential.
///
/// Wire format is a compact binary encoding (max 256 bytes).
/// String format is ``aster1<base58>``.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct AsterTicket {
    /// Endpoint ID as hex string (64 chars).
    #[pyo3(get, set)]
    pub endpoint_id: String,
    /// Relay address as "ip:port" or None.
    #[pyo3(get, set)]
    pub relay_addr: Option<String>,
    /// Direct addresses as ["ip:port", ...].
    #[pyo3(get, set)]
    pub direct_addrs: Vec<String>,
    /// Credential type: "open", "consumer_rcan", "enrollment", "registry", or None.
    #[pyo3(get, set)]
    pub credential_type: Option<String>,
    /// Credential data bytes (payload), or None.
    #[pyo3(get, set)]
    pub credential_data: Option<Vec<u8>>,
}

#[pymethods]
impl AsterTicket {
    #[new]
    #[pyo3(signature = (endpoint_id, relay_addr=None, direct_addrs=None, credential_type=None, credential_data=None))]
    fn new(
        endpoint_id: String,
        relay_addr: Option<String>,
        direct_addrs: Option<Vec<String>>,
        credential_type: Option<String>,
        credential_data: Option<Vec<u8>>,
    ) -> Self {
        Self {
            endpoint_id,
            relay_addr,
            direct_addrs: direct_addrs.unwrap_or_default(),
            credential_type,
            credential_data,
        }
    }

    /// Encode to compact binary wire format.
    fn encode(&self) -> PyResult<PyBytesResult> {
        let core = self.to_core()?;
        let bytes = core
            .encode()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(PyBytesResult(bytes))
    }

    /// Decode from compact binary wire format.
    #[staticmethod]
    fn decode(data: &[u8]) -> PyResult<Self> {
        let core = CoreTicket::decode(data)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(Self::from_core(&core))
    }

    /// Encode to ``aster1<base58>`` string.
    #[pyo3(name = "to_string")]
    fn to_string_py(&self) -> PyResult<String> {
        let core = self.to_core()?;
        core.to_base58_string()
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
    }

    /// Parse from ``aster1<base58>`` string.
    #[staticmethod]
    fn from_string(s: &str) -> PyResult<Self> {
        let core = CoreTicket::from_base58_str(s)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
        Ok(Self::from_core(&core))
    }

    fn __repr__(&self) -> String {
        format!(
            "AsterTicket(endpoint_id='{}...', relay={:?}, direct={}, cred={:?})",
            &self.endpoint_id[..8.min(self.endpoint_id.len())],
            self.relay_addr,
            self.direct_addrs.len(),
            self.credential_type,
        )
    }
}

impl AsterTicket {
    fn to_core(&self) -> PyResult<CoreTicket> {
        // Parse endpoint_id from hex
        let id_bytes = hex::decode(&self.endpoint_id).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("bad endpoint_id hex: {e}"))
        })?;
        if id_bytes.len() != 32 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "endpoint_id must be 32 bytes (64 hex chars)",
            ));
        }
        let mut endpoint_id = [0u8; 32];
        endpoint_id.copy_from_slice(&id_bytes);

        let relay = self
            .relay_addr
            .as_deref()
            .map(|s| {
                s.parse().map_err(|e| {
                    pyo3::exceptions::PyValueError::new_err(format!("bad relay addr: {e}"))
                })
            })
            .transpose()?;

        let direct_addrs = self
            .direct_addrs
            .iter()
            .map(|s| {
                s.parse().map_err(|e| {
                    pyo3::exceptions::PyValueError::new_err(format!("bad direct addr '{}': {e}", s))
                })
            })
            .collect::<PyResult<Vec<_>>>()?;

        let credential = match self.credential_type.as_deref() {
            None => None,
            Some("open") => Some(TicketCredential::Open),
            Some("consumer_rcan") => {
                let data = self.credential_data.clone().unwrap_or_default();
                Some(TicketCredential::ConsumerRcan(data))
            }
            Some("enrollment") => {
                let data = self.credential_data.clone().unwrap_or_default();
                Some(TicketCredential::Enrollment(data))
            }
            Some("registry") => {
                let data = self.credential_data.as_ref().ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err(
                        "registry credential requires 64 bytes of data (namespace_id + read_cap)",
                    )
                })?;
                if data.len() != 64 {
                    return Err(pyo3::exceptions::PyValueError::new_err(
                        "registry credential_data must be exactly 64 bytes",
                    ));
                }
                let mut namespace_id = [0u8; 32];
                let mut read_cap = [0u8; 32];
                namespace_id.copy_from_slice(&data[..32]);
                read_cap.copy_from_slice(&data[32..64]);
                Some(TicketCredential::Registry {
                    namespace_id,
                    read_cap,
                })
            }
            Some(other) => {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown credential_type '{}'",
                    other
                )));
            }
        };

        Ok(CoreTicket {
            endpoint_id,
            relay,
            direct_addrs,
            credential,
        })
    }

    fn from_core(core: &CoreTicket) -> Self {
        let (credential_type, credential_data) = match &core.credential {
            None => (None, None),
            Some(TicketCredential::Open) => (Some("open".to_string()), None),
            Some(TicketCredential::ConsumerRcan(v)) => {
                (Some("consumer_rcan".to_string()), Some(v.clone()))
            }
            Some(TicketCredential::Enrollment(v)) => {
                (Some("enrollment".to_string()), Some(v.clone()))
            }
            Some(TicketCredential::Registry {
                namespace_id,
                read_cap,
            }) => {
                let mut data = Vec::with_capacity(64);
                data.extend_from_slice(namespace_id);
                data.extend_from_slice(read_cap);
                (Some("registry".to_string()), Some(data))
            }
        };

        Self {
            endpoint_id: hex::encode(core.endpoint_id),
            relay_addr: core.relay.map(|a| a.to_string()),
            direct_addrs: core.direct_addrs.iter().map(|a| a.to_string()).collect(),
            credential_type,
            credential_data,
        }
    }
}

/// Register ticket types in the module.
pub fn register(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<AsterTicket>()?;
    Ok(())
}
