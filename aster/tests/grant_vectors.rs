#![cfg(feature = "attestation")]
//! Cross-binding golden vectors for sealed grants and ticket relay folding,
//! published as `conformance/vectors/grants.json` and
//! `conformance/vectors/ticket.json`.
//!
//! The wire-defining logic lives in `aster_transport_core::{grants, ticket}`
//! (core has its own copy of these checks); this test proves the `aster`
//! facade reproduces the same bytes and decisions through its public API.
//!
//! HPKE sealing is randomized, so the grant vectors pin what *is*
//! deterministic: AAD bytes, opening known ciphertexts (and refusing mutated
//! contexts), the role rule, grant keys, and reconcile outcomes over fixed
//! records.
//!
//! Regenerate (only for an intended, recorded format change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --test grant_vectors`

use std::path::PathBuf;

use aster::grants::{
    grant_key, open_grant, open_grant_with_key, open_namespace_grant, reconcile_grant,
    reconcile_namespace_grant, seal_grant, seal_grant_to_key, seal_namespace_grant, GrantContext,
    GrantOutcome, GrantRecord,
};
use aster::{
    hpke_public_key_from_private, Credential, NamespaceCapability, NamespaceSecret, NodeAddr,
    NodeId, SecretKey, Ticket,
};
use serde_json::{json, Value};

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors")
}

fn writing() -> bool {
    std::env::var_os("ASTER_VECTORS_WRITE").is_some()
}

fn load(name: &str) -> Value {
    let path = vectors_dir().join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("vector file is JSON")
}

fn store(name: &str, value: &Value) {
    let path = vectors_dir().join(name);
    let mut text = serde_json::to_string_pretty(value).unwrap();
    text.push('\n');
    std::fs::write(&path, text).unwrap();
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str()
        .unwrap_or_else(|| panic!("missing string {k}"))
        .to_string()
}

fn h(v: &Value, k: &str) -> Vec<u8> {
    hex::decode(s(v, k)).unwrap()
}

fn key32(v: &Value, k: &str) -> [u8; 32] {
    h(v, k).try_into().unwrap()
}

// ── Fixed identities ─────────────────────────────────────────────────────────

const ROOT_SECRET: [u8; 32] = [0x11; 32];
const RECIPIENT_SECRET: [u8; 32] = [0x22; 32];
const OTHER_SECRET: [u8; 32] = [0x33; 32];
const X25519_PRIVATE: [u8; 32] = [0x44; 32];
const NAMESPACE_SECRET: [u8; 32] = [0x55; 32];

fn node_id(secret: [u8; 32]) -> NodeId {
    NodeId::from_hex(aster::attestation::public_key(&SecretKey::from_bytes(secret)).to_hex())
}

/// Owned context fields, so vectors can be built and parsed.
#[derive(Clone)]
struct Ctx {
    app: String,
    granter: NodeId,
    recipient: NodeId,
    resource: Vec<u8>,
    path: String,
    role: String,
}

impl Ctx {
    fn borrow(&self) -> GrantContext<'_> {
        GrantContext {
            app: &self.app,
            granter: &self.granter,
            recipient: &self.recipient,
            resource: &self.resource,
            path: &self.path,
            role: &self.role,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "app": self.app,
            "granter": self.granter.as_str(),
            "recipient": self.recipient.as_str(),
            "resource_hex": hex::encode(&self.resource),
            "path": self.path,
            "role": self.role,
        })
    }

    fn from_json(v: &Value) -> Self {
        Ctx {
            app: s(v, "app"),
            granter: NodeId::from_hex(s(v, "granter")),
            recipient: NodeId::from_hex(s(v, "recipient")),
            resource: h(v, "resource_hex"),
            path: s(v, "path"),
            role: s(v, "role"),
        }
    }

    fn with(&self, f: impl FnOnce(&mut Ctx)) -> Ctx {
        let mut c = self.clone();
        f(&mut c);
        c
    }
}

fn contexts() -> Vec<(&'static str, Ctx)> {
    let root = node_id(ROOT_SECRET);
    let recipient = node_id(RECIPIENT_SECRET);
    let namespace_id = NamespaceSecret::from_bytes(NAMESPACE_SECRET).id();
    vec![
        (
            "portal-read",
            Ctx {
                app: "portal-sync/tree-grant/v1".into(),
                granter: root.clone(),
                recipient: recipient.clone(),
                resource: namespace_id.to_bytes().to_vec(),
                path: grant_key("/portal/v1", &recipient),
                role: "read".into(),
            },
        ),
        (
            "write-role",
            Ctx {
                app: "app/data-grant/v1".into(),
                granter: root.clone(),
                recipient: recipient.clone(),
                resource: namespace_id.to_bytes().to_vec(),
                path: grant_key("/app/v1", &recipient),
                role: "write".into(),
            },
        ),
        (
            "empty-fields",
            Ctx {
                app: String::new(),
                granter: root.clone(),
                recipient: recipient.clone(),
                resource: Vec::new(),
                path: String::new(),
                role: String::new(),
            },
        ),
        (
            "unicode-and-custom-role",
            Ctx {
                app: "app/gr\u{fc}n/v2".into(),
                granter: root.clone(),
                recipient: recipient.clone(),
                resource: b"tree:42".to_vec(),
                path: "/b\u{e4}ume/grants/\u{2603}".into(),
                role: "admin".into(),
            },
        ),
        (
            "self-grant-long-resource",
            Ctx {
                app: "group/identity/v1".into(),
                granter: root.clone(),
                recipient: root,
                resource: (0u8..=255).collect(),
                path: "/g/grants/self".into(),
                role: "member".into(),
            },
        ),
    ]
}

fn ctx(name: &str) -> Ctx {
    contexts().into_iter().find(|(n, _)| *n == name).unwrap().1
}

fn write_cap() -> NamespaceCapability {
    NamespaceCapability::write(NamespaceSecret::from_bytes(NAMESPACE_SECRET))
}

fn read_cap() -> NamespaceCapability {
    NamespaceCapability::read(NamespaceSecret::from_bytes(NAMESPACE_SECRET).id())
}

// ── Generation (current implementation) ──────────────────────────────────────

fn generate_grants() -> Value {
    let x_public = hpke_public_key_from_private(&X25519_PRIVATE).unwrap();

    let aad: Vec<Value> = contexts()
        .into_iter()
        .map(|(name, c)| {
            json!({ "name": name, "context": c.to_json(), "aad_hex": hex::encode(c.borrow().aad()) })
        })
        .collect();

    let grant_keys: Vec<Value> = ["/app/v1", "/app/v1/", "app///", "", "/", "portal-sync/v1"]
        .iter()
        .map(|prefix| {
            let rcpt = node_id(RECIPIENT_SECRET);
            json!({ "prefix": prefix, "recipient": rcpt.as_str(), "key": grant_key(prefix, &rcpt) })
        })
        .collect();

    let identity_open = |name: &str, c: &Ctx, plaintext: &[u8]| {
        let env = seal_grant(&c.borrow(), plaintext).unwrap();
        json!({
            "name": name,
            "scheme": "identity",
            "recipient_key_hex": hex::encode(RECIPIENT_SECRET),
            "context": c.to_json(),
            "envelope_hex": hex::encode(&env),
            "plaintext_hex": hex::encode(plaintext),
        })
    };
    let x25519_open = |name: &str, c: &Ctx, plaintext: &[u8]| {
        let env = seal_grant_to_key(&x_public, &c.borrow(), plaintext).unwrap();
        json!({
            "name": name,
            "scheme": "x25519",
            "recipient_key_hex": hex::encode(X25519_PRIVATE),
            "recipient_public_hex": hex::encode(x_public),
            "context": c.to_json(),
            "envelope_hex": hex::encode(&env),
            "plaintext_hex": hex::encode(plaintext),
        })
    };
    let portal = ctx("portal-read");
    let open = vec![
        identity_open("identity-key", &portal, b"the-secret"),
        identity_open("identity-empty-plaintext", &ctx("empty-fields"), b""),
        x25519_open(
            "x25519-key",
            &ctx("unicode-and-custom-role"),
            &(0u8..64).collect::<Vec<_>>(),
        ),
    ];

    // Each must fail to open: the context (or key, or bytes) differs from the
    // one the envelope was sealed under.
    let sealed = seal_grant(&portal.borrow(), b"the-secret").unwrap();
    let invalid = |name: &str, key: [u8; 32], c: &Ctx, env: &[u8]| {
        json!({
            "name": name,
            "scheme": "identity",
            "recipient_key_hex": hex::encode(key),
            "context": c.to_json(),
            "envelope_hex": hex::encode(env),
        })
    };
    let open_invalid = vec![
        invalid(
            "wrong-role",
            RECIPIENT_SECRET,
            &portal.with(|c| c.role = "write".into()),
            &sealed,
        ),
        invalid(
            "wrong-path",
            RECIPIENT_SECRET,
            &portal.with(|c| c.path.push('x')),
            &sealed,
        ),
        invalid(
            "wrong-app",
            RECIPIENT_SECRET,
            &portal.with(|c| c.app = "portal-sync/tree-grant/v2".into()),
            &sealed,
        ),
        invalid(
            "wrong-resource",
            RECIPIENT_SECRET,
            &portal.with(|c| c.resource[0] ^= 1),
            &sealed,
        ),
        invalid(
            "wrong-granter",
            RECIPIENT_SECRET,
            &portal.with(|c| c.granter = node_id(OTHER_SECRET)),
            &sealed,
        ),
        invalid("wrong-recipient-secret", OTHER_SECRET, &portal, &sealed),
        invalid(
            "truncated-envelope",
            RECIPIENT_SECRET,
            &portal,
            &sealed[..sealed.len() - 1],
        ),
    ];

    let write_ctx = ctx("write-role");
    let namespace_open = vec![
        json!({
            "name": "write-capability",
            "recipient_secret_hex": hex::encode(RECIPIENT_SECRET),
            "context": write_ctx.to_json(),
            "envelope_hex": hex::encode(seal_namespace_grant(&write_ctx.borrow(), &write_cap()).unwrap()),
            "capability_hex": hex::encode(write_cap().encode_fory()),
            "can_write": true,
        }),
        json!({
            "name": "read-capability",
            "recipient_secret_hex": hex::encode(RECIPIENT_SECRET),
            "context": portal.to_json(),
            "envelope_hex": hex::encode(seal_namespace_grant(&portal.borrow(), &read_cap()).unwrap()),
            "capability_hex": hex::encode(read_cap().encode_fory()),
            "can_write": false,
        }),
    ];
    // Opens under the AAD, then fails the role rule: a write capability
    // raw-sealed under a "read" context.
    let namespace_invalid = vec![json!({
        "name": "write-capability-under-read-role",
        "recipient_secret_hex": hex::encode(RECIPIENT_SECRET),
        "context": portal.to_json(),
        "envelope_hex": hex::encode(seal_grant(&portal.borrow(), &write_cap().encode_fory()).unwrap()),
    })];

    let role_rule: Vec<Value> = [
        ("read", false, true),
        ("read", true, false),
        ("write", true, true),
        ("write", false, false),
        ("admin", true, true),
        ("admin", false, true),
        ("Read", true, true),
        ("", true, true),
    ]
    .iter()
    .map(|(role, can_write, ok)| json!({ "role": role, "can_write": can_write, "ok": ok }))
    .collect();

    // Reconcile over fixed records.
    let rec = |name: &str, payload: &str, c: &Ctx, record: Option<Vec<u8>>, outcome: Value| {
        json!({
            "name": name,
            "payload": payload,
            "recipient_secret_hex": hex::encode(RECIPIENT_SECRET),
            "context": c.to_json(),
            "record_hex": record.map(hex::encode),
            "outcome": outcome,
        })
    };
    let raw_env = seal_grant(&write_ctx.borrow(), b"raw-secret").unwrap();
    let ns_env = seal_namespace_grant(&write_ctx.borrow(), &write_cap()).unwrap();
    let elsewhere = seal_grant(
        &write_ctx
            .with(|c| c.recipient = node_id(OTHER_SECRET))
            .borrow(),
        b"raw-secret",
    )
    .unwrap();
    let reconcile = vec![
        rec(
            "absent",
            "raw",
            &write_ctx,
            None,
            json!({ "kind": "absent" }),
        ),
        rec(
            "revoked",
            "namespace",
            &write_ctx,
            Some(GrantRecord::revoked("write", "node decommissioned").encode_fory()),
            json!({ "kind": "revoked", "reason": "node decommissioned", "role": "write" }),
        ),
        rec(
            "revoked-role-differs",
            "raw",
            &write_ctx,
            Some(GrantRecord::revoked("read", "").encode_fory()),
            json!({ "kind": "revoked", "reason": "", "role": "read" }),
        ),
        rec(
            "active-raw",
            "raw",
            &write_ctx,
            Some(GrantRecord::active("write", 1, raw_env.clone()).encode_fory()),
            json!({ "kind": "active", "plaintext_hex": hex::encode(b"raw-secret") }),
        ),
        rec(
            "active-raw-context-version-2",
            "raw",
            &write_ctx,
            Some(GrantRecord::active("write", 2, raw_env.clone()).encode_fory()),
            json!({ "kind": "active", "plaintext_hex": hex::encode(b"raw-secret") }),
        ),
        rec(
            "active-namespace",
            "namespace",
            &write_ctx,
            Some(GrantRecord::active("write", 1, ns_env).encode_fory()),
            json!({ "kind": "active", "capability_hex": hex::encode(write_cap().encode_fory()) }),
        ),
        rec(
            "garbage-record",
            "raw",
            &write_ctx,
            Some(b"not a grant record".to_vec()),
            json!({ "kind": "unopenable" }),
        ),
        rec(
            "record-role-mismatch",
            "raw",
            &write_ctx,
            Some(GrantRecord::active("read", 1, raw_env).encode_fory()),
            json!({ "kind": "unopenable", "error_contains": "role" }),
        ),
        rec(
            "sealed-to-another-recipient",
            "raw",
            &write_ctx,
            Some(GrantRecord::active("write", 1, elsewhere).encode_fory()),
            json!({ "kind": "unopenable" }),
        ),
    ];

    json!({
        "description": "Sealed-grant vectors (aster/sealed-grant/v1). HPKE sealing is randomized, so these pin the deterministic parts: the AAD bytes for a GrantContext, opening known envelopes (and refusing mutated contexts), the role/capability rule, the grant doc key, and reconcile decisions over fixed GrantRecord bytes. Node ids are lowercase hex Ed25519 public keys used verbatim in the AAD; 'identity' keys are 32-byte Ed25519 identity seeds (X25519 key derived as libsodium does), 'x25519' keys are raw X25519 private keys.",
        "format": {
            "aad": "for each of [\"aster/sealed-grant/v1\", app, granter, recipient, resource, path, role]: u32 BE byte length, then the bytes (strings UTF-8; granter/recipient are the node id hex strings)",
            "envelope": "Fory XLANG aster.hpke.Envelope (alg, encapped_key, ciphertext); HPKE Base mode, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305, AAD = the grant AAD",
            "grant_key": "prefix with trailing '/' trimmed, then \"/grants/\", then the recipient node id",
            "role_rule": "role \"read\" requires a read capability, \"write\" a write capability; any other role (case-sensitive) carries no implication",
            "reconcile": "absent -> absent; record not decodable -> unopenable; state revoked -> revoked{reason, role} (before any role check); record role != context role -> unopenable; envelope fails to open (or role rule fails for namespace payloads) -> unopenable; else active",
        },
        "aad": aad,
        "grant_key": grant_keys,
        "open": open,
        "open_invalid": open_invalid,
        "namespace_open": namespace_open,
        "namespace_open_invalid": namespace_invalid,
        "role_rule": role_rule,
        "reconcile": reconcile,
    })
}

const TICKET_NODE_ID: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn relay_via_ticket(url: &str) -> aster::Result<Option<String>> {
    let addr = NodeAddr {
        node_id: NodeId::from_hex(TICKET_NODE_ID),
        relay_url: Some(url.to_string()),
        direct_addresses: vec![],
    };
    Ok(Ticket::from_node_addr(&addr, Credential::Open)?.relay())
}

fn url_via_ticket(socket: &str) -> String {
    Ticket::new(
        &NodeId::from_hex(TICKET_NODE_ID),
        Some(socket),
        &[],
        Credential::Open,
    )
    .unwrap()
    .to_node_addr()
    .unwrap()
    .relay_url
    .unwrap()
}

fn generate_ticket() -> Value {
    let to_socket: Vec<Value> = [
        "https://1.2.3.4:7842",
        "https://1.2.3.4:7842/",
        "https://1.2.3.4",
        "http://1.2.3.4",
        "HTTPS://1.2.3.4:1",
        "https://1.2.3.4:443/path?q=1",
        "https://[::1]:3340",
        "https://[2001:db8::1]",
        "https://[::ffff:1.2.3.4]:5",
        "https://relay.example.com",
        "https://relay.example.com:8443/",
        "https://localhost:3340",
        "not a url",
        "",
        "foo://1.2.3.4",
        "foo://[::1]",
        "foo://[::1]:9",
    ]
    .iter()
    .map(|url| match relay_via_ticket(url) {
        Ok(socket) => json!({ "url": url, "socket": socket }),
        Err(_) => json!({ "url": url, "error": true }),
    })
    .collect();
    let to_url: Vec<Value> = [
        "1.2.3.4:7842",
        "1.2.3.4:443",
        "0.0.0.0:0",
        "[::1]:3340",
        "[2001:db8::1]:443",
    ]
    .iter()
    .map(|socket| json!({ "socket": socket, "url": url_via_ticket(socket) }))
    .collect();
    json!({
        "description": "Aster ticket relay folding. The compact ticket stores the relay as an IP socket address, not a URL. relay_url_to_socket: parse the URL (WHATWG), take an IPv4/IPv6 host and the explicit port or the scheme's default; a DNS-hostname relay yields null (dropped from the ticket, left to discovery-by-id); an unparseable URL is an error, as is an IP host with no explicit port under a scheme with no default port (a non-special scheme such as foo://1.2.3.4 has an opaque, i.e. hostname, host and so yields null). socket_to_relay_url: \"https://\" + the socket address (IPv6 bracketed).",
        "relay_url_to_socket": to_socket,
        "socket_to_relay_url": to_url,
    })
}

// ── Verification (aster public API) ─────────────────────────────────────────

fn open_by_scheme(v: &Value, c: &Ctx) -> aster::Result<Vec<u8>> {
    let env = h(v, "envelope_hex");
    let key = key32(v, "recipient_key_hex");
    let payload = match s(v, "scheme").as_str() {
        "identity" => open_grant(&SecretKey::from_bytes(key), &c.borrow(), &env)?,
        "x25519" => open_grant_with_key(&key, &c.borrow(), &env)?,
        other => panic!("unknown scheme {other}"),
    };
    Ok(payload.expose_secret().to_vec())
}

#[test]
fn grant_vectors() {
    if writing() {
        store("grants.json", &generate_grants());
    }
    let doc = load("grants.json");

    for v in doc["aad"].as_array().unwrap() {
        let c = Ctx::from_json(&v["context"]);
        assert_eq!(
            hex::encode(c.borrow().aad()),
            s(v, "aad_hex"),
            "aad {}",
            s(v, "name")
        );
    }

    for v in doc["grant_key"].as_array().unwrap() {
        assert_eq!(
            grant_key(&s(v, "prefix"), &NodeId::from_hex(s(v, "recipient"))),
            s(v, "key")
        );
    }

    for v in doc["open"].as_array().unwrap() {
        let c = Ctx::from_json(&v["context"]);
        let plaintext = open_by_scheme(v, &c).unwrap_or_else(|e| panic!("{}: {e}", s(v, "name")));
        assert_eq!(hex::encode(plaintext), s(v, "plaintext_hex"));
        if s(v, "scheme") == "x25519" {
            let public = hpke_public_key_from_private(&key32(v, "recipient_key_hex")).unwrap();
            assert_eq!(hex::encode(public), s(v, "recipient_public_hex"));
        }
    }

    for v in doc["open_invalid"].as_array().unwrap() {
        let c = Ctx::from_json(&v["context"]);
        assert!(open_by_scheme(v, &c).is_err(), "{} opened", s(v, "name"));
    }

    for v in doc["namespace_open"].as_array().unwrap() {
        let c = Ctx::from_json(&v["context"]);
        let secret = SecretKey::from_bytes(key32(v, "recipient_secret_hex"));
        let cap = open_namespace_grant(&secret, &c.borrow(), &h(v, "envelope_hex")).unwrap();
        assert_eq!(hex::encode(cap.encode_fory()), s(v, "capability_hex"));
        assert_eq!(cap.can_write(), v["can_write"].as_bool().unwrap());
    }

    for v in doc["namespace_open_invalid"].as_array().unwrap() {
        let c = Ctx::from_json(&v["context"]);
        let secret = SecretKey::from_bytes(key32(v, "recipient_secret_hex"));
        let env = h(v, "envelope_hex");
        // The envelope itself opens; the role rule is what refuses it.
        assert!(open_grant(&secret, &c.borrow(), &env).is_ok());
        let err = open_namespace_grant(&secret, &c.borrow(), &env).unwrap_err();
        assert!(matches!(err, aster::Error::InvalidArgument(_)), "{err:?}");
    }

    for v in doc["role_rule"].as_array().unwrap() {
        let role = s(v, "role");
        let cap = if v["can_write"].as_bool().unwrap() {
            write_cap()
        } else {
            read_cap()
        };
        let c = ctx("portal-read").with(|c| c.role = role.clone());
        let sealed = seal_namespace_grant(&c.borrow(), &cap);
        assert_eq!(sealed.is_ok(), v["ok"].as_bool().unwrap(), "role {role}");
        if let Err(e) = sealed {
            assert!(matches!(e, aster::Error::InvalidArgument(_)), "{e:?}");
        }
    }

    for v in doc["reconcile"].as_array().unwrap() {
        let name = s(v, "name");
        let c = Ctx::from_json(&v["context"]);
        let secret = SecretKey::from_bytes(key32(v, "recipient_secret_hex"));
        let record = v["record_hex"].as_str().map(|x| hex::decode(x).unwrap());
        let expect = &v["outcome"];
        let (kind, value, reason, role, err) = match s(v, "payload").as_str() {
            "raw" => summarize(
                reconcile_grant(&secret, &c.borrow(), record.as_deref()),
                |p| p.expose_secret().to_vec(),
            ),
            "namespace" => summarize(
                reconcile_namespace_grant(&secret, &c.borrow(), record.as_deref()),
                |cap| cap.encode_fory(),
            ),
            other => panic!("unknown payload {other}"),
        };
        assert_eq!(kind, s(expect, "kind"), "{name}");
        match kind {
            "active" => {
                let key = if expect["plaintext_hex"].is_string() {
                    "plaintext_hex"
                } else {
                    "capability_hex"
                };
                assert_eq!(hex::encode(value.unwrap()), s(expect, key), "{name}");
            }
            "revoked" => {
                assert_eq!(reason.unwrap(), s(expect, "reason"), "{name}");
                assert_eq!(role.unwrap(), s(expect, "role"), "{name}");
            }
            "unopenable" => {
                if let Some(needle) = expect["error_contains"].as_str() {
                    assert!(err.unwrap().contains(needle), "{name}");
                }
            }
            _ => {}
        }
    }
}

type Summary = (
    &'static str,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn summarize<T>(outcome: GrantOutcome<T>, bytes: impl FnOnce(T) -> Vec<u8>) -> Summary {
    let drops = outcome.drops_capability();
    let summary: Summary = match outcome {
        GrantOutcome::Active(v) => ("active", Some(bytes(v)), None, None, None),
        GrantOutcome::Revoked { reason, role } => ("revoked", None, Some(reason), Some(role), None),
        GrantOutcome::Unopenable(e) => ("unopenable", None, None, None, Some(e.to_string())),
        GrantOutcome::Absent => ("absent", None, None, None, None),
    };
    assert_eq!(drops, matches!(summary.0, "revoked" | "unopenable"));
    summary
}

#[test]
fn ticket_relay_vectors() {
    if writing() {
        store("ticket.json", &generate_ticket());
    }
    let doc = load("ticket.json");
    for v in doc["relay_url_to_socket"].as_array().unwrap() {
        let url = s(v, "url");
        let got = relay_via_ticket(&url);
        if v["error"].as_bool() == Some(true) {
            assert!(
                matches!(got, Err(aster::Error::InvalidArgument(_))),
                "{url}: {got:?}"
            );
        } else {
            assert_eq!(got.unwrap().as_deref(), v["socket"].as_str(), "{url}");
        }
    }
    for v in doc["socket_to_relay_url"].as_array().unwrap() {
        assert_eq!(url_via_ticket(&s(v, "socket")), s(v, "url"));
    }
}
