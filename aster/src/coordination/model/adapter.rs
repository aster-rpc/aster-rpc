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

/// What the group says about one resource right now (§9.1's row, read).
///
/// This is a *read*, not a guess. `LeaseSnapshot` is advisory -- nothing may
/// gate a write on it -- but advisory does not mean fabricated: an observer that
/// reports `held: false` for a resource this node itself holds is not stale, it
/// is wrong, and the lease engine's status watch surfaces it to callers as fact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceObservation {
    pub epoch: u64,
    pub held: bool,
    pub holder: Option<CoordinationNodeId>,
}

/// One committed mutation's result, with the row it left behind.
///
/// The observation travels with the reply rather than being fetched afterwards:
/// a second round trip would report a row that a concurrent command may already
/// have moved, so a denial would carry a snapshot that never explained it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendReply {
    pub outcome: Outcome,
    pub reason: Reason,
    pub fence: Option<Fence>,
    pub observed: ResourceObservation,
}

/// The coordination backend behind one committed command.
///
/// Slice 1 owns the *shape*, not the network: a transport binds this to a live
/// `CoordinationClient` in Slice 4. Keeping the trait boundary here is what lets
/// the existing `LeaseHandle` -- its renew loop, self-limit and status watch --
/// drive coordination without a second lease implementation being written.
///
/// Both methods are async and fallible, because both are round trips to a
/// quorum. A sync signature would have forced the Slice 4 implementation to
/// block a runtime thread inside the lease engine's renew loop, and an
/// infallible one would have had to render a transport failure as a coordination
/// refusal -- two different things that callers retry differently.
#[async_trait::async_trait]
pub trait CoordinationBackend: Send + Sync {
    /// Submit one committed mutation.
    async fn submit(&self, resource: &ResourceKey, op: &LeaseOp) -> anyhow::Result<BackendReply>;

    /// Read one resource row without mutating it (§5.2 `get`).
    async fn observe(&self, resource: &ResourceKey) -> anyhow::Result<ResourceObservation>;
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
            // The row is still read, because a denial's snapshot has to describe
            // the resource rather than an empty one.
            let observed = self.backend.observe(&key).await?;
            return Ok(TransitionOutcome::Denied(
                DenyReason::Unauthorized,
                snapshot_from(resource, &observed),
            ));
        }

        let reply = self.backend.submit(&key, &op).await?;
        Ok(match reply.outcome {
            Outcome::Granted => TransitionOutcome::Granted(
                reply
                    .fence
                    .ok_or_else(|| anyhow::anyhow!("a grant with no fence"))?,
            ),
            Outcome::Renewed => TransitionOutcome::Renewed,
            Outcome::Released => TransitionOutcome::Released,
            Outcome::Revoked => TransitionOutcome::Revoked,
            _ => TransitionOutcome::Denied(
                deny_reason_for(reply.reason),
                snapshot_from(resource, &reply.observed),
            ),
        })
    }

    async fn snapshot(&self, resource: &str) -> anyhow::Result<LeaseSnapshot> {
        let key = self
            .resolve(resource)
            .map_err(|e| anyhow::anyhow!("adapter resource: {e}"))?;
        let observed = self.backend.observe(&key).await?;
        Ok(snapshot_from(resource, &observed))
    }
}

/// Render an observed row as the lease engine's snapshot.
///
/// `remaining` is `None` per §9.4: a coordination lease's remaining time is not
/// a replicated fact -- `apply` reads no clock -- so reporting a number would be
/// inventing one. Everything else is copied from the row, not defaulted. An
/// earlier revision returned `epoch: 0, held: false, holder: None` for every
/// resource, which made a held lease indistinguishable from a free one to the
/// status watch that surfaces it.
fn snapshot_from(resource: &str, observed: &ResourceObservation) -> LeaseSnapshot {
    LeaseSnapshot {
        resource: resource.to_string(),
        epoch: observed.epoch,
        holder: observed.holder.as_ref().map(|h| h.to_hex()),
        held: observed.held,
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

    /// A backend with a real row behind it, so the adapter's snapshot has
    /// something true to report. Recording the calls lets a test prove the
    /// adapter read the row rather than inventing one.
    struct StubBackend {
        outcome: Outcome,
        reason: Reason,
        row: ResourceObservation,
        observes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CoordinationBackend for StubBackend {
        async fn submit(
            &self,
            _resource: &ResourceKey,
            op: &LeaseOp,
        ) -> anyhow::Result<BackendReply> {
            let fence = matches!(self.outcome, Outcome::Granted).then(|| Fence {
                resource: "r".into(),
                epoch: 1,
                holder: op.actor().to_string(),
            });
            Ok(BackendReply {
                outcome: self.outcome,
                reason: self.reason,
                fence,
                observed: self.row.clone(),
            })
        }

        async fn observe(&self, _resource: &ResourceKey) -> anyhow::Result<ResourceObservation> {
            self.observes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self.row.clone())
        }
    }

    const HOLDER: [u8; 32] = [0xC3u8; 32];

    fn held_row() -> ResourceObservation {
        ResourceObservation {
            epoch: 12,
            held: true,
            holder: Some(CoordinationNodeId::from_array(HOLDER)),
        }
    }

    fn serializer_with(
        outcome: Outcome,
        reason: Reason,
        row: ResourceObservation,
    ) -> CoordinationSerializer<StubBackend> {
        CoordinationSerializer::new(
            group(1),
            CoordinationNodeId::from_array([9u8; 32]),
            Arc::new(StubBackend {
                outcome,
                reason,
                row,
                observes: std::sync::atomic::AtomicUsize::new(0),
            }),
        )
    }

    fn serializer(outcome: Outcome, reason: Reason) -> CoordinationSerializer<StubBackend> {
        serializer_with(outcome, reason, ResourceObservation::default())
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
    /// reporting a number would be inventing one. Everything else comes from the
    /// row.
    #[tokio::test]
    async fn the_snapshot_reports_the_observed_row_not_a_default() {
        let s = serializer_with(Outcome::Granted, Reason::None, held_row());
        let resource = encode(&group(1), &key("ns", b"primary"));

        let snap = s.snapshot(&resource).await.expect("resolvable");
        assert_eq!(snap.remaining, None, "no clock, so no remaining time");
        assert_eq!(snap.resource, resource);
        assert!(snap.held, "the row is held, and the snapshot must say so");
        assert_eq!(snap.epoch, 12, "the row's epoch, not zero");
        assert_eq!(
            snap.holder,
            Some(CoordinationNodeId::from_array(HOLDER).to_hex()),
            "the holder is named, not dropped"
        );
        assert_eq!(
            s.backend
                .observes
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the row was read, not fabricated"
        );
    }

    /// A free row and a held row must produce different snapshots. This is the
    /// property an epoch-0/unheld default silently destroyed: the lease engine's
    /// status watch reported every resource as free.
    #[tokio::test]
    async fn a_free_row_and_a_held_row_do_not_look_alike() {
        let resource = encode(&group(1), &key("ns", b"primary"));
        let free = serializer_with(
            Outcome::Granted,
            Reason::None,
            ResourceObservation::default(),
        )
        .snapshot(&resource)
        .await
        .expect("resolvable");
        let held = serializer_with(Outcome::Granted, Reason::None, held_row())
            .snapshot(&resource)
            .await
            .expect("resolvable");

        assert_ne!(free, held);
        assert!(!free.held);
        assert_eq!(free.holder, None);
        assert_eq!(free.epoch, 0, "a free row genuinely has epoch zero");
    }

    /// A denial carries the row that explains it. `HeldByOther` with an empty
    /// snapshot tells a caller its lease was refused by nobody.
    #[tokio::test]
    async fn a_denial_carries_the_row_that_explains_it() {
        let s = serializer_with(Outcome::Rejected, Reason::HeldByOther, held_row());
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
        let TransitionOutcome::Denied(reason, snap) = outcome else {
            panic!("expected a denial");
        };
        assert_eq!(reason, DenyReason::HeldByOther);
        assert!(snap.held);
        assert_eq!(
            snap.holder,
            Some(CoordinationNodeId::from_array(HOLDER).to_hex()),
            "the holder that refused the acquire is named"
        );
        assert_eq!(snap.epoch, 12);
        assert_eq!(
            s.backend
                .observes
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the row travels with the reply; a second read could have moved"
        );
    }

    /// An unauthorized actor is refused without reaching the log, but the
    /// snapshot still describes the resource.
    #[tokio::test]
    async fn an_unauthorized_denial_still_describes_the_resource() {
        let s = serializer_with(Outcome::Granted, Reason::None, held_row());
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
        let TransitionOutcome::Denied(reason, snap) = outcome else {
            panic!("expected a denial");
        };
        assert_eq!(reason, DenyReason::Unauthorized);
        assert!(snap.held);
        assert_eq!(snap.epoch, 12);
    }

    /// A backend failure is an error, not a refusal. A transport fault rendered
    /// as `Denied` would tell a caller the group decided something it never saw.
    #[tokio::test]
    async fn a_backend_failure_is_not_a_refusal() {
        struct Broken;

        #[async_trait::async_trait]
        impl CoordinationBackend for Broken {
            async fn submit(
                &self,
                _resource: &ResourceKey,
                _op: &LeaseOp,
            ) -> anyhow::Result<BackendReply> {
                Err(anyhow::anyhow!("no quorum"))
            }
            async fn observe(
                &self,
                _resource: &ResourceKey,
            ) -> anyhow::Result<ResourceObservation> {
                Err(anyhow::anyhow!("no quorum"))
            }
        }

        let s = CoordinationSerializer::new(
            group(1),
            CoordinationNodeId::from_array([9u8; 32]),
            Arc::new(Broken),
        );
        let resource = encode(&group(1), &key("ns", b"primary"));
        assert!(s.snapshot(&resource).await.is_err());
        assert!(s
            .transition(
                &resource,
                LeaseOp::Revoke {
                    authority: local_actor()
                }
            )
            .await
            .is_err());
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
