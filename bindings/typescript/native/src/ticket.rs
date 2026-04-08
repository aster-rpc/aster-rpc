//! AsterTicket — NAPI wrapper for compact ticket encoding.

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::ticket::{AsterTicket as CoreTicket, TicketCredential};

use crate::error::to_napi_err;

/// Compact Aster ticket: endpoint address + optional credential.
///
/// Wire format is a compact binary encoding (max 256 bytes).
/// String format is `aster1<base58>`.
#[napi(object)]
#[derive(Clone)]
pub struct AsterTicketInfo {
    /// Endpoint ID as hex string (64 chars).
    pub endpoint_id: String,
    /// Relay address as "ip:port" or null.
    pub relay_addr: Option<String>,
    /// Direct addresses as ["ip:port", ...].
    pub direct_addrs: Vec<String>,
    /// Credential type: "open", "consumer_rcan", "enrollment", "registry", or null.
    pub credential_type: Option<String>,
    /// Credential data as hex string, or null.
    pub credential_data_hex: Option<String>,
}

/// Encode an AsterTicket to compact binary wire format.
#[napi]
pub fn aster_ticket_encode(info: AsterTicketInfo) -> Result<Buffer> {
    let core = info_to_core(&info)?;
    let bytes = core.encode().map_err(to_napi_err)?;
    Ok(Buffer::from(bytes))
}

/// Decode an AsterTicket from compact binary wire format.
#[napi]
pub fn aster_ticket_decode(data: Buffer) -> Result<AsterTicketInfo> {
    let core = CoreTicket::decode(&data).map_err(to_napi_err)?;
    Ok(core_to_info(&core))
}

/// Encode an AsterTicket to `aster1<base58>` string.
#[napi]
pub fn aster_ticket_to_string(info: AsterTicketInfo) -> Result<String> {
    let core = info_to_core(&info)?;
    core.to_base58_string().map_err(to_napi_err)
}

/// Parse an AsterTicket from `aster1<base58>` string.
#[napi]
pub fn aster_ticket_from_string(s: String) -> Result<AsterTicketInfo> {
    let core = CoreTicket::from_base58_str(&s).map_err(to_napi_err)?;
    Ok(core_to_info(&core))
}

fn info_to_core(info: &AsterTicketInfo) -> Result<CoreTicket> {
    let id_bytes = hex::decode(&info.endpoint_id)
        .map_err(|e| napi::Error::from_reason(format!("bad endpoint_id hex: {e}")))?;
    if id_bytes.len() != 32 {
        return Err(napi::Error::from_reason(
            "endpoint_id must be 32 bytes (64 hex chars)",
        ));
    }
    let mut endpoint_id = [0u8; 32];
    endpoint_id.copy_from_slice(&id_bytes);

    let relay = info
        .relay_addr
        .as_deref()
        .map(|s| {
            s.parse()
                .map_err(|e| napi::Error::from_reason(format!("bad relay addr: {e}")))
        })
        .transpose()?;

    let direct_addrs = info
        .direct_addrs
        .iter()
        .map(|s| {
            s.parse()
                .map_err(|e| napi::Error::from_reason(format!("bad direct addr '{}': {e}", s)))
        })
        .collect::<Result<Vec<_>>>()?;

    let credential = match info.credential_type.as_deref() {
        None => None,
        Some("open") => Some(TicketCredential::Open),
        Some("consumer_rcan") => {
            let data = decode_cred_hex(&info.credential_data_hex)?;
            Some(TicketCredential::ConsumerRcan(data))
        }
        Some("enrollment") => {
            let data = decode_cred_hex(&info.credential_data_hex)?;
            Some(TicketCredential::Enrollment(data))
        }
        Some("registry") => {
            let data = decode_cred_hex(&info.credential_data_hex)?;
            if data.len() != 64 {
                return Err(napi::Error::from_reason(
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
            return Err(napi::Error::from_reason(format!(
                "unknown credential_type '{other}'"
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

fn decode_cred_hex(hex_opt: &Option<String>) -> Result<Vec<u8>> {
    match hex_opt {
        None => Ok(vec![]),
        Some(h) => hex::decode(h)
            .map_err(|e| napi::Error::from_reason(format!("bad credential_data hex: {e}"))),
    }
}

fn core_to_info(core: &CoreTicket) -> AsterTicketInfo {
    let (credential_type, credential_data_hex) = match &core.credential {
        None => (None, None),
        Some(TicketCredential::Open) => (Some("open".to_string()), None),
        Some(TicketCredential::ConsumerRcan(v)) => {
            (Some("consumer_rcan".to_string()), Some(hex::encode(v)))
        }
        Some(TicketCredential::Enrollment(v)) => {
            (Some("enrollment".to_string()), Some(hex::encode(v)))
        }
        Some(TicketCredential::Registry {
            namespace_id,
            read_cap,
        }) => {
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(namespace_id);
            data.extend_from_slice(read_cap);
            (Some("registry".to_string()), Some(hex::encode(data)))
        }
    };

    AsterTicketInfo {
        endpoint_id: hex::encode(core.endpoint_id),
        relay_addr: core.relay.map(|a| a.to_string()),
        direct_addrs: core.direct_addrs.iter().map(|a| a.to_string()).collect(),
        credential_type,
        credential_data_hex,
    }
}
