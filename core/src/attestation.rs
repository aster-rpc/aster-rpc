//! Ownership-attestation chains (spec: `docs/_internal/ownership-attestations.md`).
//!
//! An attestation chain is a leaf-first bundle of edges. Each *edge* is a
//! reciprocally-signed statement pair: both the parent and the child sign the
//! same canonical body bytes. A root↔node edge says "this root owns this node"
//! and "this node is owned by this root". A verifier confirms, entirely
//! locally and with no walking, that a chain binds an expected node up to a
//! trusted anchor.
//!
//! Wire format is Apache Fory (`@WireType`-pinned, xlang). The body of each
//! edge is carried as opaque bytes and signed over with a domain separator, so
//! signatures are stable regardless of how the outer structures are framed.
//!
//! v1 is Ed25519-only; BLS is a future v2 schema bump.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use fory_core::Fory;
use fory_derive::ForyStruct;

use crate::signing::ed25519_verify;

/// Schema version carried in bodies and the chain wrapper.
pub const VERSION: i32 = 1;

/// Domain separator prepended to body bytes before signing. Scopes a signature
/// to this artifact type + schema version.
const DOMAIN: &[u8] = b"aster.attestation.v1\0";

/// Text-encoding prefix: `aster.attestation.chain.v1:<base64url>` (unpadded).
const TEXT_PREFIX: &str = "aster.attestation.chain.v1:";

/// Statement codes (u16 in the registry, widened to i32 on the Fory wire).
pub mod codes {
    pub const ROOT_OWNS_NODE: u16 = 0x0001;
    pub const NODE_OWNED_BY_ROOT: u16 = 0x0002;
    pub const ANCHOR_AUTHORISED_ROOT: u16 = 0x0010;
    pub const ROOT_AUTHORISED_BY_ANCHOR: u16 = 0x0011;
    pub const ANCHOR_AUTHORISED_INTERMEDIATE: u16 = 0x0020;
    pub const INTERMEDIATE_AUTHORISED_BY_PARENT: u16 = 0x0021;
    pub const INTERMEDIATE_OWNS_CHILD: u16 = 0x0022;
    pub const NODE_OWNED_BY_PARENT: u16 = 0x0023;
    pub const MARK_BUILD_PROVENANCE: u16 = 0x0100;
    pub const MARK_TRANSPARENCY_LOG: u16 = 0x0101;
}

/// Recognised `(parent_code, child_code)` pairs whose **child is a node** — the
/// only forms permitted for the leaf edge (`edges[0]`), which binds the node.
const NODE_TERMINAL_PAIRS: &[(u16, u16)] = &[
    (codes::ROOT_OWNS_NODE, codes::NODE_OWNED_BY_ROOT),
    (codes::INTERMEDIATE_OWNS_CHILD, codes::NODE_OWNED_BY_PARENT),
];

/// Recognised `(parent_code, child_code)` **anchor-rooted** pairs — the only
/// forms permitted for the *top* edge (`edges[-1]`), whose parent must be a
/// trusted anchor.
const ANCHOR_ROOTED_PAIRS: &[(u16, u16)] = &[
    (
        codes::ANCHOR_AUTHORISED_ROOT,
        codes::ROOT_AUTHORISED_BY_ANCHOR,
    ),
    (
        codes::ANCHOR_AUTHORISED_INTERMEDIATE,
        codes::INTERMEDIATE_AUTHORISED_BY_PARENT,
    ),
];

/// Recognised `(parent_code, child_code)` pair for an **intermediate→intermediate**
/// edge — the only form permitted for *middle* edges (`edges[1..n-1]`) in an
/// N-tier chain. (Spec: "scaling to N tiers: stack more
/// `INTERMEDIATE_OWNS_CHILD` / `INTERMEDIATE_AUTHORISED_BY_PARENT` edges".)
const INTERMEDIATE_PAIRS: &[(u16, u16)] = &[(
    codes::INTERMEDIATE_OWNS_CHILD,
    codes::INTERMEDIATE_AUTHORISED_BY_PARENT,
)];

// v1 size limits (bytes / counts). Enforced before signature verification.
const MAX_CHAIN_BYTES: usize = 4096;
const MAX_EDGE_BYTES: usize = 512;
const MAX_BODY_BYTES: usize = 256;
const MAX_CHAIN_DEPTH: usize = 8;

// ── Wire types ──────────────────────────────────────────────────────────────

/// A single claim: an identity (Ed25519 pubkey) asserting a coded role.
#[derive(ForyStruct, Debug, Clone, PartialEq)]
pub struct Statement {
    /// 32-byte Ed25519 public key == the asserting identity.
    pub id: Vec<u8>,
    /// Registry code (u16 widened to i32).
    pub code: i32,
    /// Deployment-local nuance (u16 widened to i32); opaque to outside verifiers.
    pub ext: i32,
}

/// The signed payload of an edge: a small set of statements plus replay/time bounds.
#[derive(ForyStruct, Debug, Clone, PartialEq)]
pub struct AttestationBody {
    /// Schema version (= [`VERSION`]).
    pub v: i32,
    /// Monotonic counter for replay protection (not expiry).
    pub epoch: i64,
    /// 0 = no lower bound; else reject before this unix-seconds time.
    pub not_before: i64,
    /// 0 = no expiry; else reject after this unix-seconds time.
    pub not_after: i64,
    /// 2 required (parent, child); optional 3rd Trust Mark slot.
    pub statements: Vec<Statement>,
}

/// An edge: opaque body bytes + positional signatures (`signatures[i]` by
/// `body.statements[i].id`).
#[derive(ForyStruct, Debug, Clone, PartialEq)]
pub struct AttestationEdge {
    /// Opaque Fory bytes of the [`AttestationBody`], carried verbatim & signed over.
    pub body: Vec<u8>,
    /// 64-byte Ed25519 signatures, positional with `body.statements`.
    pub signatures: Vec<Vec<u8>>,
}

/// The artifact: a bundle of opaque edge bytes, **leaf-first** (`edges[0]`'s
/// child is the node, `edges[-1]`'s parent is the anchor).
#[derive(ForyStruct, Debug, Clone, PartialEq)]
pub struct AttestationChain {
    /// Schema version (= [`VERSION`]).
    pub v: i32,
    /// Opaque Fory bytes of each [`AttestationEdge`], leaf-first to anchor-last.
    pub edges: Vec<Vec<u8>>,
}

// ── Fory runtime (built once, shared) ────────────────────────────────────────

static FORY: OnceLock<Fory> = OnceLock::new();

/// The shared, pre-registered Fory runtime. Built once; safe to share by `&`.
/// (Fory holds immutable config + the type registry; per-call buffers are
/// thread-local.) Re-initialising per call is a known perf footgun, so callers
/// must always go through this.
fn fory() -> &'static Fory {
    FORY.get_or_init(|| {
        let mut f = Fory::builder().xlang(true).compatible(true).build();
        // Register leaf-to-root: Statement is nested inside AttestationBody.
        f.register_by_name::<Statement>("aster.attestation.statement.v1")
            .expect("register Statement");
        f.register_by_name::<AttestationBody>("aster.attestation.body.v1")
            .expect("register AttestationBody");
        f.register_by_name::<AttestationEdge>("aster.attestation.edge.v1")
            .expect("register AttestationEdge");
        f.register_by_name::<AttestationChain>("aster.attestation.chain.v1")
            .expect("register AttestationChain");
        f
    })
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Failure modes for attesting and verifying.
#[derive(Debug, thiserror::Error)]
pub enum AttestationError {
    #[error("encode error: {0}")]
    Encode(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("artifact exceeds size limit")]
    TooLarge,
    #[error("unsupported version: {0}")]
    Version(i32),
    #[error("invalid chain depth: {0}")]
    BadDepth(usize),
    #[error("statement count out of range: {0}")]
    BadStatements(usize),
    #[error("statement code or ext out of u16 range")]
    CodeOutOfRange,
    #[error("disallowed edge form: ({0:#06x}, {1:#06x})")]
    DisallowedEdgeForm(u16, u16),
    #[error("signature count does not match statement count")]
    SigCountMismatch,
    #[error("statement id is not 32 bytes")]
    BadStatementId,
    #[error("signature is not 64 bytes")]
    BadSignatureLen,
    #[error("attestation not yet valid")]
    NotYetValid,
    #[error("attestation expired")]
    Expired,
    #[error("leaf does not bind to the expected node")]
    NodeMismatch,
    #[error("chain bridging is broken")]
    BrokenChain,
    #[error("top anchor is not in the trusted set")]
    UntrustedAnchor,
    #[error("signature verification failed")]
    BadSignature,
    #[error("crypto error: {0}")]
    Crypto(String),
    #[error("invalid text encoding")]
    BadText,
}

/// Crate-local result alias for attestation operations.
pub type Result<T> = std::result::Result<T, AttestationError>;

// ── Mint ─────────────────────────────────────────────────────────────────────

/// Policy options for a minted edge.
#[derive(Debug, Clone, Default)]
pub struct AttestOptions {
    /// Monotonic replay counter (caller-managed; 0 is fine for a first issue).
    pub epoch: i64,
    /// 0 = no lower bound; else unix seconds.
    pub not_before: i64,
    /// 0 = no expiry; else unix seconds.
    pub not_after: i64,
}

/// Mint a single-edge root↔node chain: the root attests it owns the node, and
/// the node co-signs. Both secrets are 32-byte Ed25519 seeds. Returns the Fory
/// wire bytes of the [`AttestationChain`].
pub fn attest_root_node(
    root_secret: &[u8; 32],
    node_secret: &[u8; 32],
    opts: &AttestOptions,
) -> Result<Vec<u8>> {
    let root_sk = SigningKey::from_bytes(root_secret);
    let node_sk = SigningKey::from_bytes(node_secret);
    let root_pk = root_sk.verifying_key().to_bytes().to_vec();
    let node_pk = node_sk.verifying_key().to_bytes().to_vec();

    let body = AttestationBody {
        v: VERSION,
        epoch: opts.epoch,
        not_before: opts.not_before,
        not_after: opts.not_after,
        statements: vec![
            Statement {
                id: root_pk,
                code: codes::ROOT_OWNS_NODE as i32,
                ext: 0,
            },
            Statement {
                id: node_pk,
                code: codes::NODE_OWNED_BY_ROOT as i32,
                ext: 0,
            },
        ],
    };

    let body_bytes = fory()
        .serialize(&body)
        .map_err(|e| AttestationError::Encode(e.to_string()))?;
    let si = sign_input(&body_bytes);
    let edge = AttestationEdge {
        body: body_bytes,
        signatures: vec![
            root_sk.sign(&si).to_bytes().to_vec(),
            node_sk.sign(&si).to_bytes().to_vec(),
        ],
    };
    let edge_bytes = fory()
        .serialize(&edge)
        .map_err(|e| AttestationError::Encode(e.to_string()))?;
    let chain = AttestationChain {
        v: VERSION,
        edges: vec![edge_bytes],
    };
    let chain_bytes = fory()
        .serialize(&chain)
        .map_err(|e| AttestationError::Encode(e.to_string()))?;
    if chain_bytes.len() > MAX_CHAIN_BYTES {
        return Err(AttestationError::TooLarge);
    }
    Ok(chain_bytes)
}

/// Derive the 32-byte Ed25519 public key from a 32-byte secret seed.
pub fn public_key(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

// ── Verify ───────────────────────────────────────────────────────────────────

/// The outcome of a successful verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChain {
    /// The leaf node the chain binds to (== `expected_node`).
    pub node: [u8; 32],
    /// The trusted anchor the chain terminates at.
    pub anchor: [u8; 32],
    /// Intermediate identities strictly between the node and the anchor,
    /// ordered node-ward → anchor-ward. Empty for a 1-edge chain.
    pub intermediates: Vec<[u8; 32]>,
    /// Number of edges (chain depth).
    pub depth: usize,
}

/// Verify a chain end-to-end. The only validation entry point — there is no
/// "signatures only" path. Confirms (in order): size bounds, structural
/// well-formedness, that the leaf binds to `expected_node`, that each upper
/// edge bridges to the one below, that the top anchor is in `trusted_anchors`,
/// and finally every signature. All checks are local and CPU-only.
pub fn verify_chain(
    chain_bytes: &[u8],
    trusted_anchors: &[[u8; 32]],
    expected_node: &[u8; 32],
) -> Result<VerifiedChain> {
    // 0. Pre-parser size gate (O(1), before the Fory decoder runs).
    if chain_bytes.len() > MAX_CHAIN_BYTES {
        return Err(AttestationError::TooLarge);
    }

    // 1. Decode the chain wrapper.
    let chain: AttestationChain = fory()
        .deserialize(chain_bytes)
        .map_err(|e| AttestationError::Decode(e.to_string()))?;
    if chain.v != VERSION {
        return Err(AttestationError::Version(chain.v));
    }
    let n = chain.edges.len();
    if n == 0 || n > MAX_CHAIN_DEPTH {
        return Err(AttestationError::BadDepth(n));
    }

    // 2. Decode + structurally check each edge/body.
    let now = now_unix();
    let mut bodies: Vec<AttestationBody> = Vec::with_capacity(n);
    let mut edges: Vec<AttestationEdge> = Vec::with_capacity(n);
    let mut sign_inputs: Vec<Vec<u8>> = Vec::with_capacity(n);
    for edge_bytes in &chain.edges {
        if edge_bytes.len() > MAX_EDGE_BYTES {
            return Err(AttestationError::TooLarge);
        }
        let edge: AttestationEdge = fory()
            .deserialize(edge_bytes)
            .map_err(|e| AttestationError::Decode(e.to_string()))?;
        if edge.body.len() > MAX_BODY_BYTES {
            return Err(AttestationError::TooLarge);
        }
        let body: AttestationBody = fory()
            .deserialize(&edge.body)
            .map_err(|e| AttestationError::Decode(e.to_string()))?;
        if body.v != VERSION {
            return Err(AttestationError::Version(body.v));
        }
        let s = body.statements.len();
        if !(2..=3).contains(&s) {
            return Err(AttestationError::BadStatements(s));
        }
        if edge.signatures.len() != s {
            return Err(AttestationError::SigCountMismatch);
        }
        for st in &body.statements {
            if st.id.len() != 32 {
                return Err(AttestationError::BadStatementId);
            }
            // `code`/`ext` are int32 on the wire but MUST fit u16 (registry range).
            if !(0..=0xFFFF).contains(&st.code) || !(0..=0xFFFF).contains(&st.ext) {
                return Err(AttestationError::CodeOutOfRange);
            }
        }
        for sig in &edge.signatures {
            if sig.len() != 64 {
                return Err(AttestationError::BadSignatureLen);
            }
        }
        if body.not_before != 0 && now < body.not_before {
            return Err(AttestationError::NotYetValid);
        }
        if body.not_after != 0 && now > body.not_after {
            return Err(AttestationError::Expired);
        }
        sign_inputs.push(sign_input(&edge.body));
        bodies.push(body);
        edges.push(edge);
    }

    // 3. Chain-level structural well-formedness.
    // Edge-form (shape) validation by position — the N-tier grammar:
    //   leaf (edges[0])      : node-ownership form (binds the node)
    //   middle (edges[1..-1]): intermediate→intermediate
    //   top (edges[-1])      : anchor-rooted authorisation form
    // (When n == 1 the single edge is the leaf, rooted directly at the anchor.)
    // Without this, a validly-signed but semantically-wrong edge (e.g. an
    // authorisation pair) could bind `expected_node` as if it were *owned*.
    for (i, body) in bodies.iter().enumerate() {
        let pair = (
            body.statements[0].code as u16,
            body.statements[1].code as u16,
        );
        let allowed: &[(u16, u16)] = if i == 0 {
            NODE_TERMINAL_PAIRS
        } else if i == n - 1 {
            ANCHOR_ROOTED_PAIRS
        } else {
            INTERMEDIATE_PAIRS
        };
        if !allowed.contains(&pair) {
            return Err(AttestationError::DisallowedEdgeForm(pair.0, pair.1));
        }
    }
    // Leaf binds to the expected node (child slot of the leaf edge).
    if bodies[0].statements[1].id.as_slice() != expected_node.as_slice() {
        return Err(AttestationError::NodeMismatch);
    }
    // Each upper edge's child (slot 1) must equal the lower edge's parent (slot 0).
    for i in 0..n - 1 {
        if bodies[i + 1].statements[1].id != bodies[i].statements[0].id {
            return Err(AttestationError::BrokenChain);
        }
    }
    // The top edge's parent (slot 0) must be a trusted anchor.
    let top_anchor = &bodies[n - 1].statements[0].id;
    if !trusted_anchors
        .iter()
        .any(|a| a.as_slice() == top_anchor.as_slice())
    {
        return Err(AttestationError::UntrustedAnchor);
    }

    // 4. Crypto: verify every signature (reciprocal, positional).
    for (idx, edge) in edges.iter().enumerate() {
        let si = &sign_inputs[idx];
        for (i, st) in bodies[idx].statements.iter().enumerate() {
            let ok = ed25519_verify(&st.id, si, &edge.signatures[i])
                .map_err(|e| AttestationError::Crypto(e.to_string()))?;
            if !ok {
                return Err(AttestationError::BadSignature);
            }
        }
    }

    let mut node = [0u8; 32];
    node.copy_from_slice(&bodies[0].statements[1].id);
    let mut anchor = [0u8; 32];
    anchor.copy_from_slice(top_anchor);
    // Intermediates = the parent (slot 0) of every edge except the top edge,
    // ordered node-ward → anchor-ward.
    let mut intermediates = Vec::with_capacity(n.saturating_sub(1));
    for body in &bodies[..n - 1] {
        let mut id = [0u8; 32];
        id.copy_from_slice(&body.statements[0].id);
        intermediates.push(id);
    }
    Ok(VerifiedChain {
        node,
        anchor,
        intermediates,
        depth: n,
    })
}

// ── Text encoding ─────────────────────────────────────────────────────────────

/// Encode chain wire bytes as `aster.attestation.chain.v1:<base64url>` (unpadded).
pub fn encode_text(chain_bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "{TEXT_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(chain_bytes)
    )
}

/// Decode an `aster.attestation.chain.v1:<base64url>` string back to wire bytes.
pub fn decode_text(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    let b64 = s
        .strip_prefix(TEXT_PREFIX)
        .ok_or(AttestationError::BadText)?;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|_| AttestationError::BadText)
}

// ── helpers ────────────────────────────────────────────────────────────────────

fn sign_input(body_bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(DOMAIN.len() + body_bytes.len());
    v.extend_from_slice(DOMAIN);
    v.extend_from_slice(body_bytes);
    v
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn keypair(seed: u8) -> ([u8; 32], [u8; 32]) {
        let secret = [seed; 32];
        let pk = SigningKey::from_bytes(&secret).verifying_key().to_bytes();
        (secret, pk)
    }

    /// Mint a correctly-signed single edge with caller-chosen statement codes,
    /// so verification fails (if at all) only on code/shape validation.
    fn mint_single_edge(
        root_secret: &[u8; 32],
        node_secret: &[u8; 32],
        parent_code: i32,
        child_code: i32,
    ) -> Vec<u8> {
        let root_sk = SigningKey::from_bytes(root_secret);
        let node_sk = SigningKey::from_bytes(node_secret);
        let body = AttestationBody {
            v: VERSION,
            epoch: 0,
            not_before: 0,
            not_after: 0,
            statements: vec![
                Statement {
                    id: root_sk.verifying_key().to_bytes().to_vec(),
                    code: parent_code,
                    ext: 0,
                },
                Statement {
                    id: node_sk.verifying_key().to_bytes().to_vec(),
                    code: child_code,
                    ext: 0,
                },
            ],
        };
        let body_bytes = fory().serialize(&body).unwrap();
        let si = sign_input(&body_bytes);
        let edge = AttestationEdge {
            body: body_bytes,
            signatures: vec![
                root_sk.sign(&si).to_bytes().to_vec(),
                node_sk.sign(&si).to_bytes().to_vec(),
            ],
        };
        let edge_bytes = fory().serialize(&edge).unwrap();
        let chain = AttestationChain {
            v: VERSION,
            edges: vec![edge_bytes],
        };
        fory().serialize(&chain).unwrap()
    }

    #[test]
    fn rejects_wrong_or_unknown_slot_codes() {
        let (root_sk, root_pk) = keypair(1);
        let (node_sk, node_pk) = keypair(2);

        // Correct codes verify.
        let ok = mint_single_edge(
            &root_sk,
            &node_sk,
            codes::ROOT_OWNS_NODE as i32,
            codes::NODE_OWNED_BY_ROOT as i32,
        );
        assert!(verify_chain(&ok, &[root_pk], &node_pk).is_ok());

        // Child slot carries the wrong (parent) code.
        let wrong_child = mint_single_edge(
            &root_sk,
            &node_sk,
            codes::ROOT_OWNS_NODE as i32,
            codes::ROOT_OWNS_NODE as i32,
        );
        assert!(matches!(
            verify_chain(&wrong_child, &[root_pk], &node_pk).unwrap_err(),
            AttestationError::DisallowedEdgeForm(..)
        ));

        // An authorisation pair cannot stand as the leaf (node-binding) edge.
        let auth_as_leaf = mint_single_edge(
            &root_sk,
            &node_sk,
            codes::ANCHOR_AUTHORISED_ROOT as i32,
            codes::ROOT_AUTHORISED_BY_ANCHOR as i32,
        );
        assert!(matches!(
            verify_chain(&auth_as_leaf, &[root_pk], &node_pk).unwrap_err(),
            AttestationError::DisallowedEdgeForm(..)
        ));

        // Unknown codes are rejected at slot 0/1.
        let unknown = mint_single_edge(&root_sk, &node_sk, 0x00AA, 0x00BB);
        assert!(matches!(
            verify_chain(&unknown, &[root_pk], &node_pk).unwrap_err(),
            AttestationError::DisallowedEdgeForm(..)
        ));
    }

    /// Build a correctly-signed multi-edge chain from `(parent_secret,
    /// child_secret, parent_code, child_code)` specs, leaf-first.
    fn build_chain(specs: &[(&[u8; 32], &[u8; 32], i32, i32)]) -> Vec<u8> {
        let mut edge_bytes_list = Vec::with_capacity(specs.len());
        for (parent_secret, child_secret, pcode, ccode) in specs {
            let p = SigningKey::from_bytes(parent_secret);
            let c = SigningKey::from_bytes(child_secret);
            let body = AttestationBody {
                v: VERSION,
                epoch: 0,
                not_before: 0,
                not_after: 0,
                statements: vec![
                    Statement {
                        id: p.verifying_key().to_bytes().to_vec(),
                        code: *pcode,
                        ext: 0,
                    },
                    Statement {
                        id: c.verifying_key().to_bytes().to_vec(),
                        code: *ccode,
                        ext: 0,
                    },
                ],
            };
            let body_bytes = fory().serialize(&body).unwrap();
            let si = sign_input(&body_bytes);
            let edge = AttestationEdge {
                body: body_bytes,
                signatures: vec![
                    p.sign(&si).to_bytes().to_vec(),
                    c.sign(&si).to_bytes().to_vec(),
                ],
            };
            edge_bytes_list.push(fory().serialize(&edge).unwrap());
        }
        let chain = AttestationChain {
            v: VERSION,
            edges: edge_bytes_list,
        };
        fory().serialize(&chain).unwrap()
    }

    #[test]
    fn verifies_three_edge_intermediate_chain() {
        // anchor → int1 → int2 → node (4-tier topology, 3 edges).
        let (anchor_sk, anchor_pk) = keypair(10);
        let (int1_sk, int1_pk) = keypair(11);
        let (int2_sk, int2_pk) = keypair(12);
        let (node_sk, node_pk) = keypair(13);

        let chain = build_chain(&[
            // leaf: int2 owns node
            (
                &int2_sk,
                &node_sk,
                codes::INTERMEDIATE_OWNS_CHILD as i32,
                codes::NODE_OWNED_BY_PARENT as i32,
            ),
            // middle: int1 → int2 (intermediate→intermediate)
            (
                &int1_sk,
                &int2_sk,
                codes::INTERMEDIATE_OWNS_CHILD as i32,
                codes::INTERMEDIATE_AUTHORISED_BY_PARENT as i32,
            ),
            // top: anchor authorises int1
            (
                &anchor_sk,
                &int1_sk,
                codes::ANCHOR_AUTHORISED_INTERMEDIATE as i32,
                codes::INTERMEDIATE_AUTHORISED_BY_PARENT as i32,
            ),
        ]);

        let v = verify_chain(&chain, &[anchor_pk], &node_pk).unwrap();
        assert_eq!(v.depth, 3);
        assert_eq!(v.node, node_pk);
        assert_eq!(v.anchor, anchor_pk);
        // Intermediates ordered node-ward → anchor-ward.
        assert_eq!(v.intermediates, vec![int2_pk, int1_pk]);
    }

    #[test]
    fn rejects_three_edge_with_wrong_middle_form() {
        let (anchor_sk, anchor_pk) = keypair(10);
        let (int1_sk, _) = keypair(11);
        let (int2_sk, _) = keypair(12);
        let (node_sk, node_pk) = keypair(13);

        // Middle edge uses an anchor-rooted (top) form instead of
        // intermediate→intermediate — must be rejected by the positional grammar.
        let chain = build_chain(&[
            (
                &int2_sk,
                &node_sk,
                codes::INTERMEDIATE_OWNS_CHILD as i32,
                codes::NODE_OWNED_BY_PARENT as i32,
            ),
            (
                &int1_sk,
                &int2_sk,
                codes::ANCHOR_AUTHORISED_INTERMEDIATE as i32,
                codes::INTERMEDIATE_AUTHORISED_BY_PARENT as i32,
            ),
            (
                &anchor_sk,
                &int1_sk,
                codes::ANCHOR_AUTHORISED_INTERMEDIATE as i32,
                codes::INTERMEDIATE_AUTHORISED_BY_PARENT as i32,
            ),
        ]);

        assert!(matches!(
            verify_chain(&chain, &[anchor_pk], &node_pk).unwrap_err(),
            AttestationError::DisallowedEdgeForm(..)
        ));
    }

    #[test]
    fn rejects_out_of_range_code() {
        let (root_sk, root_pk) = keypair(1);
        let (node_sk, node_pk) = keypair(2);
        // 0x10000 exceeds u16 range.
        let oversized = mint_single_edge(
            &root_sk,
            &node_sk,
            0x10000,
            codes::NODE_OWNED_BY_ROOT as i32,
        );
        assert!(matches!(
            verify_chain(&oversized, &[root_pk], &node_pk).unwrap_err(),
            AttestationError::CodeOutOfRange
        ));
    }

    #[test]
    fn round_trip_and_verify() {
        let (root_sk, root_pk) = keypair(1);
        let (node_sk, node_pk) = keypair(2);
        let chain = attest_root_node(&root_sk, &node_sk, &AttestOptions::default()).unwrap();

        let v = verify_chain(&chain, &[root_pk], &node_pk).unwrap();
        assert_eq!(v.node, node_pk);
        assert_eq!(v.anchor, root_pk);
        assert_eq!(v.depth, 1);
    }

    #[test]
    fn rejects_untrusted_anchor() {
        let (root_sk, _root_pk) = keypair(1);
        let (node_sk, node_pk) = keypair(2);
        let (_other_sk, other_pk) = keypair(9);
        let chain = attest_root_node(&root_sk, &node_sk, &AttestOptions::default()).unwrap();
        let err = verify_chain(&chain, &[other_pk], &node_pk).unwrap_err();
        assert!(matches!(err, AttestationError::UntrustedAnchor));
    }

    #[test]
    fn rejects_wrong_node() {
        let (root_sk, root_pk) = keypair(1);
        let (node_sk, _node_pk) = keypair(2);
        let (_x_sk, wrong_node) = keypair(7);
        let chain = attest_root_node(&root_sk, &node_sk, &AttestOptions::default()).unwrap();
        let err = verify_chain(&chain, &[root_pk], &wrong_node).unwrap_err();
        assert!(matches!(err, AttestationError::NodeMismatch));
    }

    #[test]
    fn rejects_expired() {
        let (root_sk, root_pk) = keypair(1);
        let (node_sk, node_pk) = keypair(2);
        let opts = AttestOptions {
            epoch: 0,
            not_before: 0,
            not_after: 1, // 1970 — long past
        };
        let chain = attest_root_node(&root_sk, &node_sk, &opts).unwrap();
        let err = verify_chain(&chain, &[root_pk], &node_pk).unwrap_err();
        assert!(matches!(err, AttestationError::Expired));
    }

    #[test]
    fn rejects_tampered_signature() {
        let (root_sk, root_pk) = keypair(1);
        let (node_sk, node_pk) = keypair(2);
        let mut chain = attest_root_node(&root_sk, &node_sk, &AttestOptions::default()).unwrap();
        // Flip a byte near the end (inside a signature region) and expect failure.
        let last = chain.len() - 1;
        chain[last] ^= 0xFF;
        assert!(verify_chain(&chain, &[root_pk], &node_pk).is_err());
    }

    #[test]
    fn text_round_trip() {
        let (root_sk, root_pk) = keypair(3);
        let (node_sk, node_pk) = keypair(4);
        let chain = attest_root_node(&root_sk, &node_sk, &AttestOptions::default()).unwrap();
        let text = encode_text(&chain);
        assert!(text.starts_with("aster.attestation.chain.v1:"));
        let back = decode_text(&text).unwrap();
        assert_eq!(back, chain);
        verify_chain(&back, &[root_pk], &node_pk).unwrap();
    }
}
