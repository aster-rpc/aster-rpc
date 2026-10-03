//! Grant record framing — the container a sealed grant is published in.
//!
//! A sealed grant's HPKE envelope never sits bare at a policy-doc key. It is
//! wrapped in a `GrantRecord`, which carries in **plaintext** the two things a
//! recipient must know *before* it can open anything:
//!
//! - **`state`** — [`GrantState::Revoked`] is an explicit value, never a
//!   deleted key. Under eventual replication an absent key is ambiguous with
//!   "hasn't synced yet", so a reconcile loop that reads absence as revocation
//!   tears the node down during its first sync; and a delete leans on tombstone
//!   propagation to beat any replica still serving the old entry. An explicit
//!   state arrives unambiguously, carries a `reason`, and is auditable.
//! - **`context_version`** — which `GrantContext` template the envelope was
//!   sealed under. The app label lives *inside* the AAD, so without a plaintext
//!   version the only way to migrate an AAD layout is trial decryption. This
//!   field is what keeps version selection out of the crypto.
//!
//! A revoked record is required to carry an empty envelope, so revocation can
//! never ship openable material by accident.

use anyhow::{anyhow, bail, Result};
use fory_core::Fory;
use fory_derive::ForyStruct;
use std::fmt;
use std::sync::OnceLock;

/// Schema version for Fory XLANG grant record bytes.
pub const GRANT_RECORD_VERSION: i32 = 1;
/// Fory XLANG state value for [`GrantState::Active`].
pub const GRANT_STATE_ACTIVE: i32 = 0;
/// Fory XLANG state value for [`GrantState::Revoked`].
pub const GRANT_STATE_REVOKED: i32 = 1;

#[derive(ForyStruct, Clone, PartialEq)]
pub struct GrantRecordWire {
    #[fory(id = 1)]
    pub v: i32,
    #[fory(id = 2)]
    pub state: i32,
    #[fory(id = 3)]
    pub context_version: i32,
    #[fory(id = 4)]
    pub role: String,
    #[fory(id = 5, bytes)]
    pub envelope: Vec<u8>,
    #[fory(id = 6)]
    pub reason: String,
}

impl fmt::Debug for GrantRecordWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrantRecordWire")
            .field("v", &self.v)
            .field("state", &self.state)
            .field("context_version", &self.context_version)
            .field("role", &self.role)
            .field("envelope", &format_args!("<{} bytes>", self.envelope.len()))
            .field("reason", &self.reason)
            .finish()
    }
}

static FORY: OnceLock<Fory> = OnceLock::new();

fn fory() -> &'static Fory {
    FORY.get_or_init(|| {
        let mut f = crate::xlang::new_fory();
        f.register_by_name::<GrantRecordWire>("aster.grants.GrantRecord")
            .expect("register GrantRecordWire");
        f
    })
}

/// Lifecycle state of a grant record. Revocation is this value, **not** a
/// deleted doc key — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantState {
    /// The grant is in force; `envelope` carries the sealed capability.
    Active,
    /// The grant is withdrawn. The envelope is empty; `reason` says why.
    Revoked,
}

/// A root-authored grant record: the plaintext framing around a sealed
/// envelope, published at the conventional grant key `<prefix>/grants/<node_id>`.
#[derive(Clone, PartialEq, Eq)]
pub struct GrantRecord {
    /// Active or revoked. Never encode revocation as an absent key.
    pub state: GrantState,
    /// Which `GrantContext` template the envelope was sealed under. Read this
    /// *before* opening to select the context; never trial-decrypt.
    pub context_version: i32,
    /// The grant role (`"read"` / `"write"`, or an app scheme), matching the
    /// sealing context's role. Retained on revoked records for display.
    pub role: String,
    /// The Fory-encoded HPKE envelope. Always empty when revoked.
    pub envelope: Vec<u8>,
    /// Revocation reason. Empty on active records.
    pub reason: String,
}

impl GrantRecord {
    /// An active record carrying `envelope` (the output of `seal_grant`).
    pub fn active(role: impl Into<String>, context_version: i32, envelope: Vec<u8>) -> Self {
        Self {
            state: GrantState::Active,
            context_version,
            role: role.into(),
            envelope,
            reason: String::new(),
        }
    }

    /// A revocation record. Write this at the grant key; do **not** delete the
    /// key. The role is retained for display but carries no capability.
    pub fn revoked(role: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            state: GrantState::Revoked,
            context_version: GRANT_RECORD_VERSION,
            role: role.into(),
            envelope: Vec::new(),
            reason: reason.into(),
        }
    }

    /// True if this record grants authority right now.
    pub fn is_active(&self) -> bool {
        self.state == GrantState::Active
    }

    /// Fory XLANG binary encoding.
    pub fn encode_fory(&self) -> Vec<u8> {
        let wire = GrantRecordWire {
            v: GRANT_RECORD_VERSION,
            state: match self.state {
                GrantState::Active => GRANT_STATE_ACTIVE,
                GrantState::Revoked => GRANT_STATE_REVOKED,
            },
            context_version: self.context_version,
            role: self.role.clone(),
            envelope: self.envelope.clone(),
            reason: self.reason.clone(),
        };
        fory()
            .serialize(&wire)
            .expect("GrantRecordWire Fory serialization should not fail")
    }

    /// Decode Fory XLANG grant record bytes.
    ///
    /// Rejects a revoked record that carries an envelope (revocation must not
    /// ship openable material) and an active record that carries none.
    pub fn decode_fory(bytes: &[u8]) -> Result<Self> {
        let wire: GrantRecordWire = fory()
            .deserialize(bytes)
            .map_err(|e| anyhow!("grant record Fory decode failed: {e}"))?;
        if wire.v != GRANT_RECORD_VERSION {
            bail!("grant record version must be 1, got {}", wire.v);
        }

        let state = match wire.state {
            GRANT_STATE_ACTIVE => GrantState::Active,
            GRANT_STATE_REVOKED => GrantState::Revoked,
            other => bail!("grant record state must be 0(active) or 1(revoked), got {other}"),
        };
        match state {
            GrantState::Active if wire.envelope.is_empty() => {
                bail!("active grant record must carry an envelope")
            }
            GrantState::Revoked if !wire.envelope.is_empty() => {
                bail!("revoked grant record must not carry an envelope")
            }
            _ => {}
        }

        Ok(Self {
            state,
            context_version: wire.context_version,
            role: wire.role,
            envelope: wire.envelope,
            reason: wire.reason,
        })
    }
}

impl fmt::Debug for GrantRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrantRecord")
            .field("state", &self.state)
            .field("context_version", &self.context_version)
            .field("role", &self.role)
            .field("envelope", &format_args!("<{} bytes>", self.envelope.len()))
            .field("reason", &self.reason)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_record_round_trips() {
        let active = GrantRecord::active("write", 1, vec![9u8; 48]);
        assert_eq!(
            GrantRecord::decode_fory(&active.encode_fory()).unwrap(),
            active
        );

        let revoked = GrantRecord::revoked("write", "node decommissioned");
        let decoded = GrantRecord::decode_fory(&revoked.encode_fory()).unwrap();
        assert_eq!(decoded, revoked);
        assert!(!decoded.is_active());
        assert_eq!(decoded.reason, "node decommissioned");
    }

    #[test]
    fn revoked_record_may_not_carry_an_envelope() {
        // Hand-built: the constructors can't produce this, a buggy or hostile
        // writer can.
        let wire = GrantRecordWire {
            v: GRANT_RECORD_VERSION,
            state: GRANT_STATE_REVOKED,
            context_version: 1,
            role: "write".into(),
            envelope: vec![1, 2, 3],
            reason: String::new(),
        };
        let err = GrantRecord::decode_fory(&fory().serialize(&wire).unwrap()).unwrap_err();
        assert!(err.to_string().contains("must not carry an envelope"));
    }

    #[test]
    fn active_record_must_carry_an_envelope() {
        let wire = GrantRecordWire {
            v: GRANT_RECORD_VERSION,
            state: GRANT_STATE_ACTIVE,
            context_version: 1,
            role: "read".into(),
            envelope: Vec::new(),
            reason: String::new(),
        };
        let err = GrantRecord::decode_fory(&fory().serialize(&wire).unwrap()).unwrap_err();
        assert!(err.to_string().contains("must carry an envelope"));
    }

    #[test]
    fn decode_rejects_unknown_state_and_version() {
        let bad_state = GrantRecordWire {
            v: GRANT_RECORD_VERSION,
            state: 7,
            context_version: 1,
            role: "read".into(),
            envelope: vec![1],
            reason: String::new(),
        };
        let err = GrantRecord::decode_fory(&fory().serialize(&bad_state).unwrap()).unwrap_err();
        assert!(err.to_string().contains("state must be"));

        let bad_version = GrantRecordWire {
            v: 99,
            state: GRANT_STATE_ACTIVE,
            context_version: 1,
            role: "read".into(),
            envelope: vec![1],
            reason: String::new(),
        };
        let err = GrantRecord::decode_fory(&fory().serialize(&bad_version).unwrap()).unwrap_err();
        assert!(err.to_string().contains("version must be 1"));
    }

    #[test]
    fn context_version_is_readable_without_opening_the_envelope() {
        let record = GrantRecord::active("write", 2, vec![4u8; 16]);
        let decoded = GrantRecord::decode_fory(&record.encode_fory()).unwrap();
        assert_eq!(decoded.context_version, 2);
    }

    #[test]
    fn debug_does_not_print_envelope_bytes() {
        let text = format!("{:?}", GrantRecord::active("write", 1, vec![0xabu8; 8]));
        assert!(text.contains("<8 bytes>"));
        assert!(!text.contains("ab"));
    }
}
