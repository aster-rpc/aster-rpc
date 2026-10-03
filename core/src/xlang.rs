//! The one place an Aster Fory runtime is configured.
//!
//! Every binding pins the same Apache Fory release (1.7.6) and the same
//! options for payloads and for core's Fory-encoded artefacts (attestation
//! chains, namespace capabilities, HPKE envelopes, grant records, topology
//! records, coordination roots):
//!
//! | option     | value | why |
//! |------------|-------|-----|
//! | xlang      | on    | cross-language wire format |
//! | compatible | on    | named-compatible structs; field add/remove tolerated |
//! | ref        | off   | no shared/cyclic graphs on the wire; smaller, simpler bytes |
//!
//! The Fory options feed the encoded bytes (and, in non-compatible mode, the
//! struct hash), so a drift between producer and consumer is a wire break.
//! Build runtimes only through [`new_fory`].
//!
//! The RPC envelope (`StreamHeader` / `CallHeader` / `RpcStatus`) is **not**
//! Fory-encoded (Aster-SPEC §6.9).

/// The pinned Fory release, for diagnostics and cross-binding checks.
pub const FORY_VERSION: &str = "1.7.6";

/// A fresh Fory runtime with Aster's standard options (xlang on, compatible
/// on, reference tracking off). Register types into it, then share it by `&`:
/// construction is the expensive part.
pub fn new_fory() -> fory_core::Fory {
    fory_core::Fory::builder()
        .xlang(true)
        .compatible(true)
        .track_ref(false)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_options() {
        let f = new_fory();
        assert!(f.is_xlang());
        assert!(f.is_compatible());
        assert!(!f.config().track_ref);
    }
}
