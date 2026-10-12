//! Client for the @aster directory service (`aster-app`): handle claims and
//! verification, service publication and discovery, and access grants.
//!
//! The service is written in Python and speaks Fory XLANG over Aster RPC.
//! [`wire`] mirrors its types and [`services`] declares its three services,
//! from which `#[aster::service]` generates typed clients. Mutations are
//! [`payload`] bodies signed by the profile's root key ([`Signer`]) over the
//! exact JSON bytes Python produces ([`pyjson`]).

pub mod manifest;
pub mod payload;
pub mod pyjson;
pub mod services;
pub mod wire;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use aster::rpc::RpcConnection;
use aster::{AsterConfig, Node, NodeId, Ticket};
use ed25519_dalek::{Signer as _, SigningKey};

use crate::payload::Payload;
use crate::pyjson::{DumpOptions, PyJson};
pub use crate::services::{AccessServiceClient, ProfileServiceClient, PublicationServiceClient};
use crate::wire::SignedRequest;

/// Overrides `[aster_service] node_id` in the configuration.
pub const ADDRESS_ENV: &str = "ASTER_SERVICE_ADDR";

/// The service address: `explicit` (a flag), else `ASTER_SERVICE_ADDR`, else
/// the configured `[aster_service] node_id`.
pub fn resolve_address(explicit: Option<&str>, config: &aster_config::Config) -> Result<String> {
    if let Some(a) = explicit.filter(|a| !a.trim().is_empty()) {
        return Ok(a.trim().to_string());
    }
    if let Ok(a) = std::env::var(ADDRESS_ENV)
        && !a.trim().is_empty()
    {
        return Ok(a.trim().to_string());
    }
    if !config.aster_service.enabled {
        bail!(
            "the @aster service is disabled in this configuration ([aster_service] enabled = false)"
        );
    }
    match &config.aster_service.node_id {
        Some(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        _ => bail!("no @aster service address configured"),
    }
}

/// Signs request bodies as a profile's root key.
pub struct Signer {
    key: SigningKey,
    pubkey_hex: String,
}

impl Signer {
    pub fn new(seed: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(seed);
        let pubkey_hex = hex(&key.verifying_key().to_bytes());
        Self { key, pubkey_hex }
    }

    pub fn public_key_hex(&self) -> &str {
        &self.pubkey_hex
    }

    /// Add `action`, the current `timestamp` and a fresh `nonce` to `body`
    /// and sign the result.
    pub fn sign<P: Payload>(&self, body: &P) -> Result<SignedRequest> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| anyhow::anyhow!("OS random number generator: {e}"))?;
        self.sign_at(body, now as i64, &hex(&nonce))
    }

    fn sign_at<P: Payload>(&self, body: &P, timestamp: i64, nonce: &str) -> Result<SignedRequest> {
        let PyJson::Object(mut fields) = PyJson::from_serialize(body)? else {
            bail!("a request body serialises to a JSON object");
        };
        fields.push(("action".into(), PyJson::Str(P::ACTION.into())));
        fields.push(("timestamp".into(), PyJson::Int(timestamp.into())));
        fields.push(("nonce".into(), PyJson::Str(nonce.into())));
        let payload_json = PyJson::Object(fields).dumps(DumpOptions::SIGNING)?;
        let signature = self.key.sign(payload_json.as_bytes());
        Ok(SignedRequest {
            payload_json,
            signer_pubkey: self.pubkey_hex.clone(),
            signature: hex(&signature.to_bytes()),
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A connection to the @aster service and its three typed clients.
pub struct Directory {
    pub profile: ProfileServiceClient,
    pub publication: PublicationServiceClient,
    pub access: AccessServiceClient,
    _node: Node,
}

impl Directory {
    /// Connect from a fresh, in-memory node with an ephemeral identity:
    /// requests are authenticated by their signatures, not by the QUIC peer.
    /// `address` is an `aster1…` ticket or a 64-hex node ID.
    pub async fn connect(address: &str) -> Result<Self> {
        let node = Node::start(AsterConfig::builder().build())
            .await
            .context("starting a local node")?;
        let peer = if address.starts_with("aster1") {
            node.add_ticket_addr(&Ticket::from_base58(address)?)?
        } else if address.len() == 64 && address.bytes().all(|b| b.is_ascii_hexdigit()) {
            NodeId::from_hex(address.to_ascii_lowercase())
        } else {
            bail!("an @aster address is an `aster1…` ticket or a 64-character node ID");
        };
        let conn: RpcConnection = node
            .rpc_connect(&peer)
            .await
            .with_context(|| format!("connecting to @aster at {address}"))?;
        Ok(Self {
            profile: ProfileServiceClient::new(conn.clone()),
            publication: PublicationServiceClient::new(conn.clone()),
            access: AccessServiceClient::new(conn),
            _node: node,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::GrantAccessPayload;

    #[test]
    fn signs_the_python_canonical_form() {
        // RFC 8032 test vector 1 key.
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            let h = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
            *b = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap();
        }
        let signer = Signer::new(&seed);
        assert_eq!(
            signer.public_key_hex(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        let body = GrantAccessPayload {
            handle: "ops".into(),
            service_name: "Échos".into(),
            consumer_handle: "bob".into(),
            role: "consumer".into(),
            scope: "handle".into(),
            scope_node_id: None,
        };
        let req = signer
            .sign_at(&body, 1_700_000_000, &"ab".repeat(16))
            .unwrap();
        // json.dumps(asdict(GrantAccessPayload(...)), sort_keys=True,
        // separators=(",", ":"), ensure_ascii=False) in CPython 3.13.
        assert_eq!(
            req.payload_json,
            r#"{"action":"grant_access","consumer_handle":"bob","handle":"ops","nonce":"abababababababababababababababab","role":"consumer","scope":"handle","scope_node_id":null,"service_name":"Échos","timestamp":1700000000}"#
        );
        let sig = ed25519_dalek::Signature::from_slice(
            &(0..64)
                .map(|i| u8::from_str_radix(&req.signature[2 * i..2 * i + 2], 16).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        signer
            .key
            .verifying_key()
            .verify_strict(req.payload_json.as_bytes(), &sig)
            .unwrap();
    }
}
