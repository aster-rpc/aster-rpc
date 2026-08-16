//! The `CoordinationSerializer` adapter (§9.4): the coordination model behind
//! the existing `core::lease::LeaseSerializer` trait.
//!
//! Slice 1's job here is the **compatibility proof**, not the network client.
//! The adapter binds one group and namespace, encodes a coordination resource
//! key into the trait's `&str` resource, and maps coordination outcomes onto
//! `TransitionOutcome` -- so an existing `LeaseHandle` drives the coordination
//! state machine without a second lease implementation. Wiring it to a live
//! `CoordinationClient` is Slice 4's.
//!
//! ## The canonical adapter encoding
//!
//! ```text
//! ac1:<group-id-hex>:<namespace>:<base64url-no-pad(resource-key-bytes)>
//! ```
//!
//! The encoded **group** stays inside the string that `core::Fence.resource`
//! carries into the renew loop. That is what stops a handle held across a
//! recovery from being reinterpreted against a recovered group: the fence names
//! the group it was granted in, so a serializer bound to a different one rejects
//! it rather than renewing a lease that no longer exists.
//!
//! Non-canonical input is rejected rather than normalised. Two spellings of one
//! resource would be two leases over one row.

use std::fmt;
use std::sync::Arc;

use aster_transport_core::lease::{
    DenyReason, Fence, LeaseOp, LeaseSerializer, LeaseSnapshot, TransitionOutcome,
};

use crate::coordination::ids::{CoordinationGroupId, CoordinationNodeId, ResourceKey};
use crate::coordination::reason::{Outcome, Reason};

/// The adapter-encoding prefix.
const PREFIX: &str = "ac1";

/// A parsed adapter resource: the group, namespace and key it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterResource {
    pub group: CoordinationGroupId,
    pub key: ResourceKey,
}

/// Why an adapter resource string was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    /// Not four colon-separated fields beginning `ac1`.
    Shape,
    /// The group id was not 32 hex-encoded bytes.
    Group,
    /// The base64url segment did not decode, or was padded.
    Encoding,
    /// The decoded `(namespace, key)` failed §4.2 validation.
    Key,
    /// The string named a group other than the one this serializer is bound to.
    ///
    /// Separate from `Group` because it is not malformed -- it is a valid
    /// resource belonging to somebody else, which is exactly the recovered-group
    /// case the encoding exists to catch.
    WrongGroup,
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            AdapterError::Shape => "not a canonical ac1 adapter resource",
            AdapterError::Group => "the group id is not 32 hex bytes",
            AdapterError::Encoding => "the key segment is not unpadded base64url",
            AdapterError::Key => "the decoded resource key is not valid",
            AdapterError::WrongGroup => "the resource belongs to another group",
        };
        f.write_str(text)
    }
}

/// Encode a coordination resource as the adapter string.
pub fn encode(group: &CoordinationGroupId, key: &ResourceKey) -> String {
    format!(
        "{PREFIX}:{}:{}:{}",
        group.to_hex(),
        key.namespace(),
        base64url_nopad(key.key())
    )
}

/// Parse an adapter string, requiring it to name `expected_group`.
pub fn decode(
    s: &str,
    expected_group: &CoordinationGroupId,
) -> Result<AdapterResource, AdapterError> {
    // `splitn(4, ..)` deliberately, not `split`: a namespace cannot contain a
    // colon (§4.2's charset forbids it) but a *malformed* one might, and the
    // key segment is base64url, which also cannot. Four fields exactly.
    let mut parts = s.split(':');
    let (Some(prefix), Some(group_hex), Some(namespace), Some(key_b64), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(AdapterError::Shape);
    };
    if prefix != PREFIX {
        return Err(AdapterError::Shape);
    }

    let group = CoordinationGroupId::parse_hex(group_hex).map_err(|_| AdapterError::Group)?;
    if &group != expected_group {
        return Err(AdapterError::WrongGroup);
    }

    let key_bytes = decode_base64url_nopad(key_b64).ok_or(AdapterError::Encoding)?;
    let key = ResourceKey::new(namespace, key_bytes).map_err(|_| AdapterError::Key)?;
    Ok(AdapterResource { group, key })
}

/// Unpadded base64url, written out rather than pulled in: the alphabet is four
/// lines and a dependency for it would be a dependency in the shipping graph.
fn base64url_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let indices = [n >> 18, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (i, index) in indices.iter().enumerate() {
            // Emit only the characters the input length justifies, so the
            // output is unpadded rather than padded-then-trimmed.
            if i <= chunk.len() {
                out.push(ALPHABET[*index as usize] as char);
            }
        }
    }
    out
}

fn decode_base64url_nopad(s: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'-' => 62,
            b'_' => 63,
            // Padding is not canonical here; neither is standard base64's
            // `+`/`/`. Both are rejected rather than accepted-and-normalised.
            _ => return None,
        })
    }

    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= value(*c)? << (18 - 6 * i);
        }

        // **Canonical tail bits.** A short final quantum encodes 8 or 16 bits
        // in 12 or 18, and the spare low bits MUST be zero. Without this check
        // several distinct strings decode to the same bytes -- `YQ`, `YR`, `Yf`
        // and thirteen more all give `[0x61]` -- so one resource would have
        // sixteen spellings and therefore sixteen lease identities over one
        // row. Rejecting is the only safe answer: normalising would silently
        // accept a peer's non-canonical form and hand back a different string
        // than it sent.
        let spare = match chunk.len() {
            2 => n & 0xFFFF,
            3 => n & 0xFF,
            _ => 0,
        };
        if spare != 0 {
            return None;
        }

        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The coordination backend behind one committed command.
///
/// Slice 1 owns the *shape*, not the network: a transport binds this to a live
/// `CoordinationClient` in Slice 4. Keeping the trait boundary here is what lets
/// the existing `LeaseHandle` -- its renew loop, self-limit and status watch --
/// drive coordination without a second lease implementation being written.
#[allow(async_fn_in_trait)]
pub trait CoordinationBackend: Send + Sync {
    /// Submit one committed mutation and return `(outcome, reason, fence)`.
    fn submit(&self, resource: &ResourceKey, op: &LeaseOp) -> (Outcome, Reason, Option<Fence>);
}

/// `CoordinationSerializer` (§9.4): the coordination group behind the existing
/// `LeaseSerializer` trait.
///
/// Binds **one** group and one local NodeId. Both bindings are enforced on every
/// call rather than assumed:
///
/// * the adapter resource must name this group, so a handle held across a
///   recovery cannot be renewed against the recovered one;
/// * every `LeaseOp`'s actor string must equal this client's immutable local
///   NodeId. The wire omits the actor -- it comes from transport identity -- so
///   an actor string that disagrees means the caller believes it is someone
///   else, and proceeding would attribute its writes to the wrong node.
pub struct CoordinationSerializer<B: CoordinationBackend> {
    group: CoordinationGroupId,
    local_node: CoordinationNodeId,
    backend: Arc<B>,
}

impl<B: CoordinationBackend> CoordinationSerializer<B> {
    pub fn new(
        group: CoordinationGroupId,
        local_node: CoordinationNodeId,
        backend: Arc<B>,
    ) -> Self {
        Self {
            group,
            local_node,
            backend,
        }
    }

    /// Resolve an adapter resource, enforcing the group binding.
    pub fn resolve(&self, resource: &str) -> Result<ResourceKey, AdapterError> {
        decode(resource, &self.group).map(|r| r.key)
    }

    /// Every `LeaseOp` actor must be this client's own NodeId.
    fn actor_is_local(&self, op: &LeaseOp) -> bool {
        op.actor() == self.local_node.to_hex()
    }
}

/// Map a coordination refusal onto the lease engine's `DenyReason`.
///
/// The mapping is deliberately lossy in one direction only: `LeaseSerializer`
/// predates coordination and has four reasons, so several coordination reasons
/// share one. Authorization refusals map to `Unauthorized`, which is what §9.4
/// specifies -- a client that needs the exact code uses `CoordinationLease`
/// rather than the compatibility path.
pub fn deny_reason_for(reason: Reason) -> DenyReason {
    match reason {
        Reason::HeldByOther => DenyReason::HeldByOther,
        Reason::NotHeld => DenyReason::NotHeld,
        Reason::NotHolder => DenyReason::NotHolder,
        Reason::EpochMismatch => DenyReason::EpochMismatch,
        _ => DenyReason::Unauthorized,
    }
}

#[async_trait::async_trait]
impl<B: CoordinationBackend + 'static> LeaseSerializer for CoordinationSerializer<B> {
    async fn transition(&self, resource: &str, op: LeaseOp) -> anyhow::Result<TransitionOutcome> {
        let key = self
            .resolve(resource)
            .map_err(|e| anyhow::anyhow!("adapter resource: {e}"))?;

        if !self.actor_is_local(&op) {
            // Not a refusal the group decided -- the caller is claiming an
            // identity this client does not have, so it never reaches the log.
            return Ok(TransitionOutcome::Denied(
                DenyReason::Unauthorized,
                advisory_snapshot(resource),
            ));
        }

        let (outcome, reason, fence) = self.backend.submit(&key, &op);
        Ok(match outcome {
            Outcome::Granted => TransitionOutcome::Granted(
                fence.ok_or_else(|| anyhow::anyhow!("a grant with no fence"))?,
            ),
            Outcome::Renewed => TransitionOutcome::Renewed,
            Outcome::Released => TransitionOutcome::Released,
            Outcome::Revoked => TransitionOutcome::Revoked,
            _ => TransitionOutcome::Denied(deny_reason_for(reason), advisory_snapshot(resource)),
        })
    }

    async fn snapshot(&self, resource: &str) -> anyhow::Result<LeaseSnapshot> {
        self.resolve(resource)
            .map_err(|e| anyhow::anyhow!("adapter resource: {e}"))?;
        Ok(advisory_snapshot(resource))
    }
}

/// §9.4: the advisory snapshot's `remaining` is `None`.
///
/// A coordination lease's remaining time is not a replicated fact -- it depends
/// on a clock, and `apply` reads none -- so reporting a number would be
/// inventing one.
fn advisory_snapshot(resource: &str) -> LeaseSnapshot {
    LeaseSnapshot {
        resource: resource.to_string(),
        epoch: 0,
        holder: None,
        held: false,
        remaining: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(byte: u8) -> CoordinationGroupId {
        CoordinationGroupId::from_array([byte; 32])
    }

    fn key(ns: &str, k: &[u8]) -> ResourceKey {
        ResourceKey::new(ns, k).expect("valid")
    }

    #[test]
    fn an_adapter_resource_round_trips() {
        let g = group(0xAB);
        let k = key("portal.roaming", b"\x00\xff primary");
        let encoded = encode(&g, &k);

        assert!(encoded.starts_with("ac1:"));
        let decoded = decode(&encoded, &g).expect("round trip");
        assert_eq!(decoded.group, g);
        assert_eq!(decoded.key, k);
    }

    /// Keys of every length modulo 3, so the unpadded encoding is exercised on
    /// all three tail shapes.
    #[test]
    fn every_key_length_round_trips_without_padding() {
        let g = group(1);
        for len in 1..=32usize {
            let k = key("ns", &vec![0xA5u8; len]);
            let encoded = encode(&g, &k);
            assert!(!encoded.contains('='), "the encoding must be unpadded");
            assert_eq!(decode(&encoded, &g).expect("round trip").key, k);
        }
    }

    /// The encoded group stays in the string the renew loop carries, so a
    /// handle from another group cannot be reinterpreted against this one.
    #[test]
    fn a_resource_from_another_group_is_rejected() {
        let mine = group(1);
        let theirs = group(2);
        let encoded = encode(&theirs, &key("ns", b"k"));

        assert_eq!(decode(&encoded, &mine), Err(AdapterError::WrongGroup));
        // And it is *valid* for its own group, which is what makes this a
        // wrong-group rejection rather than a parse failure.
        assert!(decode(&encoded, &theirs).is_ok());
    }

    /// Non-canonical input is rejected, not normalised: two spellings of one
    /// resource would be two leases over one row.
    #[test]
    fn non_canonical_input_is_rejected() {
        let g = group(1);
        let valid = encode(&g, &key("ns", b"key"));

        let cases: Vec<(&str, String, AdapterError)> = vec![
            (
                "no prefix",
                valid.replacen("ac1:", "", 1),
                AdapterError::Shape,
            ),
            (
                "wrong prefix",
                valid.replacen("ac1:", "ac2:", 1),
                AdapterError::Shape,
            ),
            (
                "too few fields",
                "ac1:aa:bb".to_string(),
                AdapterError::Shape,
            ),
            (
                "too many fields",
                format!("{valid}:extra"),
                AdapterError::Shape,
            ),
            (
                "short group",
                format!("ac1:{}:ns:a2V5", "ab".repeat(31)),
                AdapterError::Group,
            ),
            (
                "non-hex group",
                format!("ac1:{}:ns:a2V5", "zz".repeat(32)),
                AdapterError::Group,
            ),
            (
                "padded key",
                format!("ac1:{}:ns:a2V5=", group(1).to_hex()),
                AdapterError::Encoding,
            ),
            (
                "standard base64 alphabet",
                format!("ac1:{}:ns:a+V5", group(1).to_hex()),
                AdapterError::Encoding,
            ),
            (
                "invalid namespace",
                format!("ac1:{}:NS:a2V5", group(1).to_hex()),
                AdapterError::Key,
            ),
            (
                "empty key",
                format!("ac1:{}:ns:", group(1).to_hex()),
                AdapterError::Key,
            ),
        ];

        for (name, input, expected) in cases {
            assert_eq!(decode(&input, &g), Err(expected), "{name}");
        }
    }

    /// Non-canonical **tail bits** are rejected. Without this, sixteen distinct
    /// strings decode to `[0x61]`, so one row would have sixteen lease
    /// identities -- and the lease engine compares resources as opaque strings.
    #[test]
    fn non_canonical_tail_bits_are_rejected() {
        // In a two-character quantum the second symbol carries 4 spare bits, so
        // exactly four of the 64 symbols are canonical -- the ones whose low
        // nibble is zero, giving `A`, `Q`, `g`, `w` for bytes 0x60..=0x63.
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut canonical = 0;
        for (index, symbol) in ALPHABET.iter().enumerate() {
            let s = format!("Y{}", *symbol as char);
            let decoded = decode_base64url_nopad(&s);
            if index % 16 == 0 {
                canonical += 1;
                assert!(decoded.is_some(), "{s} is the canonical form of a byte");
            } else {
                assert_eq!(
                    decoded, None,
                    "{s} sets spare tail bits and is not canonical"
                );
            }
        }
        assert_eq!(
            canonical, 4,
            "sixty of the sixty-four spellings are refused"
        );
        assert_eq!(decode_base64url_nopad("YQ"), Some(vec![0x61]));

        // The three-character quantum has 2 spare bits, same rule.
        let canonical = base64url_nopad(&[0x61, 0x62]);
        assert_eq!(decode_base64url_nopad(&canonical), Some(vec![0x61, 0x62]));
        let mut bad = canonical.clone().into_bytes();
        *bad.last_mut().unwrap() += 1;
        assert_eq!(
            decode_base64url_nopad(std::str::from_utf8(&bad).unwrap()),
            None
        );
    }

    /// The consequence, stated as the property that matters: no two distinct
    /// adapter strings name the same resource.
    #[test]
    fn one_resource_has_exactly_one_adapter_spelling() {
        let g = group(1);
        for len in 1..=8usize {
            let k = key("ns", &vec![0x61u8; len]);
            let canonical = encode(&g, &k);
            // Perturbing the last character either changes the key or is
            // refused -- never silently decodes to the same row.
            let mut bytes = canonical.clone().into_bytes();
            let last = bytes.last_mut().unwrap();
            *last = if *last == b'_' { b'-' } else { *last + 1 };
            let perturbed = String::from_utf8(bytes).unwrap();
            if let Ok(other) = decode(&perturbed, &g) {
                assert_ne!(
                    other.key, k,
                    "{perturbed} decoded to the same resource as {canonical}"
                );
            }
        }
    }

    // ── The LeaseSerializer implementation ──────────────────────────────────

    struct StubBackend {
        outcome: Outcome,
        reason: Reason,
    }

    impl CoordinationBackend for StubBackend {
        fn submit(
            &self,
            _resource: &ResourceKey,
            op: &LeaseOp,
        ) -> (Outcome, Reason, Option<Fence>) {
            let fence = matches!(self.outcome, Outcome::Granted).then(|| Fence {
                resource: "r".into(),
                epoch: 1,
                holder: op.actor().to_string(),
            });
            (self.outcome, self.reason, fence)
        }
    }

    fn serializer(outcome: Outcome, reason: Reason) -> CoordinationSerializer<StubBackend> {
        CoordinationSerializer::new(
            group(1),
            CoordinationNodeId::from_array([9u8; 32]),
            Arc::new(StubBackend { outcome, reason }),
        )
    }

    fn local_actor() -> String {
        CoordinationNodeId::from_array([9u8; 32]).to_hex()
    }

    /// The adapter really implements the trait, so an existing `LeaseHandle`
    /// drives coordination without a second lease implementation.
    #[tokio::test]
    async fn the_adapter_implements_lease_serializer() {
        let s = serializer(Outcome::Granted, Reason::None);
        let resource = encode(&group(1), &key("ns", b"primary"));

        let outcome = s
            .transition(
                &resource,
                LeaseOp::Acquire {
                    candidate: local_actor(),
                    ttl: std::time::Duration::from_secs(30),
                },
            )
            .await
            .expect("no backend error");
        assert!(matches!(outcome, TransitionOutcome::Granted(_)));
    }

    /// A resource from another group is refused, so a handle held across a
    /// recovery cannot be renewed against the recovered group -- the reason the
    /// group is inside the string the renew loop carries.
    #[tokio::test]
    async fn a_resource_from_another_group_is_refused_by_the_serializer() {
        let s = serializer(Outcome::Granted, Reason::None);
        let theirs = encode(&group(2), &key("ns", b"primary"));
        assert!(s
            .transition(
                &theirs,
                LeaseOp::Revoke {
                    authority: local_actor()
                }
            )
            .await
            .is_err());
        assert!(s.snapshot(&theirs).await.is_err());
    }

    /// Every `LeaseOp` actor must equal this client's own NodeId. The wire omits
    /// the actor -- it comes from transport identity -- so a disagreeing actor
    /// string means the caller believes it is someone else.
    #[tokio::test]
    async fn an_op_from_another_actor_is_unauthorized() {
        let s = serializer(Outcome::Granted, Reason::None);
        let resource = encode(&group(1), &key("ns", b"primary"));

        let outcome = s
            .transition(
                &resource,
                LeaseOp::Renew {
                    holder: "not-this-node".into(),
                    epoch: 1,
                },
            )
            .await
            .expect("no backend error");
        assert!(matches!(
            outcome,
            TransitionOutcome::Denied(DenyReason::Unauthorized, _)
        ));
    }

    /// Coordination refusals map onto the lease engine's reasons, with
    /// authorization becoming `Unauthorized` per §9.4.
    #[test]
    fn coordination_reasons_map_onto_deny_reasons() {
        assert_eq!(
            deny_reason_for(Reason::HeldByOther),
            DenyReason::HeldByOther
        );
        assert_eq!(deny_reason_for(Reason::NotHeld), DenyReason::NotHeld);
        assert_eq!(deny_reason_for(Reason::NotHolder), DenyReason::NotHolder);
        assert_eq!(
            deny_reason_for(Reason::EpochMismatch),
            DenyReason::EpochMismatch
        );
        for reason in [
            Reason::PermissionChanged,
            Reason::StateBudgetExhausted,
            Reason::ClientRetired,
        ] {
            assert_eq!(
                deny_reason_for(reason),
                DenyReason::Unauthorized,
                "{reason:?} has no lease-engine equivalent"
            );
        }
    }

    /// §9.4: the advisory `remaining` is `None`, because a coordination lease's
    /// remaining time is not a replicated fact -- `apply` reads no clock, so
    /// reporting a number would be inventing one.
    #[tokio::test]
    async fn the_advisory_snapshot_reports_no_remaining_time() {
        let s = serializer(Outcome::Granted, Reason::None);
        let resource = encode(&group(1), &key("ns", b"primary"));
        let snap = s.snapshot(&resource).await.expect("resolvable");
        assert_eq!(snap.remaining, None);
        assert_eq!(snap.resource, resource);
    }

    /// The encoding is stable: the same resource always produces the same
    /// string, which is what lets it be compared as an opaque `&str` by the
    /// existing lease engine.
    #[test]
    fn the_encoding_is_deterministic() {
        let g = group(7);
        let k = key("ns", b"abc");
        assert_eq!(encode(&g, &k), encode(&g, &k));
        // And a golden value, so a change to the encoding is visible rather
        // than silently accepted by a round trip against itself.
        assert_eq!(encode(&g, &k), format!("ac1:{}:ns:YWJj", "07".repeat(32)));
    }
}
