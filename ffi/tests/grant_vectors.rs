//! The C ABI grant functions (ABI 3.4) against
//! `conformance/vectors/grants.json`: AAD bytes, grant keys, opening the
//! pinned envelopes (identity and explicit-key schemes) and re-sealing them,
//! the invalid-open cases, namespace grants and the role rule, and every
//! reconcile outcome. Everything goes through the `extern "C"` functions.

use std::path::PathBuf;
use std::{ptr, slice};

use aster_transport_ffi::data::aster_local_access_t;
use aster_transport_ffi::grants::*;
use aster_transport_ffi::pure::*;
use aster_transport_ffi::{aster_bytes_t, aster_status_t, aster_string_release};
use serde_json::Value;

const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const VERIFY: i32 = aster_status_t::ASTER_STATUS_VERIFICATION_FAILED as i32;

fn vectors() -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors/grants.json");
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str()
        .unwrap_or_else(|| panic!("missing string {k}"))
}

fn h(v: &Value, k: &str) -> Vec<u8> {
    hex::decode(s(v, k)).unwrap()
}

fn b(x: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: if x.is_empty() {
            ptr::null()
        } else {
            x.as_ptr()
        },
        len: x.len(),
    }
}

fn empty() -> aster_bytes_t {
    aster_bytes_t {
        ptr: ptr::null(),
        len: 0,
    }
}

unsafe fn view(x: aster_bytes_t) -> Vec<u8> {
    if x.len == 0 {
        Vec::new()
    } else {
        slice::from_raw_parts(x.ptr, x.len).to_vec()
    }
}

unsafe fn take(x: aster_bytes_t) -> Vec<u8> {
    let v = view(x);
    assert_eq!(aster_string_release(x.ptr, x.len), OK);
    v
}

/// Owned context fields; `c()` borrows them as the C struct.
struct Ctx {
    app: String,
    granter: String,
    recipient: String,
    resource: Vec<u8>,
    path: String,
    role: String,
}

impl Ctx {
    fn from_json(v: &Value) -> Self {
        Ctx {
            app: s(v, "app").into(),
            granter: s(v, "granter").into(),
            recipient: s(v, "recipient").into(),
            resource: h(v, "resource_hex"),
            path: s(v, "path").into(),
            role: s(v, "role").into(),
        }
    }

    fn c(&self) -> aster_grant_context_t {
        aster_grant_context_t {
            struct_size: std::mem::size_of::<aster_grant_context_t>() as u32,
            reserved: 0,
            app: b(self.app.as_bytes()),
            granter: b(self.granter.as_bytes()),
            recipient: b(self.recipient.as_bytes()),
            resource: b(&self.resource),
            path: b(self.path.as_bytes()),
            role: b(self.role.as_bytes()),
        }
    }
}

unsafe fn open_by_scheme(v: &Value, ctx: &Ctx) -> (i32, Vec<u8>) {
    let env = h(v, "envelope_hex");
    let key = h(v, "recipient_key_hex");
    let c = ctx.c();
    let mut out = empty();
    let rc = match s(v, "scheme") {
        "identity" => aster_grant_open(key.as_ptr(), &c, b(&env), &mut out),
        "x25519" => aster_grant_open_with_key(key.as_ptr(), &c, b(&env), &mut out),
        other => panic!("unknown scheme {other}"),
    };
    (rc, take(out))
}

#[test]
fn aad_vectors() {
    let doc = vectors();
    for v in doc["aad"].as_array().unwrap() {
        let ctx = Ctx::from_json(&v["context"]);
        let mut out = empty();
        unsafe {
            assert_eq!(aster_grant_context_aad(&ctx.c(), &mut out), OK);
            assert_eq!(hex::encode(take(out)), s(v, "aad_hex"), "{}", s(v, "name"));
        }
    }
    // A wrong struct_size is refused.
    let ctx = Ctx::from_json(&doc["aad"][0]["context"]);
    let mut c = ctx.c();
    c.struct_size = 8;
    let mut out = empty();
    unsafe {
        assert_eq!(aster_grant_context_aad(&c, &mut out), INVALID);
        assert!(out.ptr.is_null());
    }
}

#[test]
fn grant_key_vectors() {
    for v in vectors()["grant_key"].as_array().unwrap() {
        let mut out = empty();
        unsafe {
            assert_eq!(
                aster_grant_key(
                    b(s(v, "prefix").as_bytes()),
                    b(s(v, "recipient").as_bytes()),
                    &mut out
                ),
                OK
            );
            assert_eq!(String::from_utf8(take(out)).unwrap(), s(v, "key"));
        }
    }
}

#[test]
fn open_vectors() {
    let doc = vectors();
    for v in doc["open"].as_array().unwrap() {
        let name = s(v, "name");
        let ctx = Ctx::from_json(&v["context"]);
        let c = ctx.c();
        unsafe {
            let (rc, plaintext) = open_by_scheme(v, &ctx);
            assert_eq!(rc, OK, "{name}");
            assert_eq!(hex::encode(&plaintext), s(v, "plaintext_hex"), "{name}");

            // Re-seal (randomized) and open again.
            let key = h(v, "recipient_key_hex");
            let mut env = empty();
            let mut opened = empty();
            if s(v, "scheme") == "identity" {
                assert_eq!(aster_grant_seal(&c, b(&plaintext), &mut env), OK);
                let env = take(env);
                assert_eq!(aster_grant_open(key.as_ptr(), &c, b(&env), &mut opened), OK);
            } else {
                let mut public = [0u8; 32];
                assert_eq!(
                    aster_hpke_public_key_from_private(key.as_ptr(), public.as_mut_ptr()),
                    OK
                );
                assert_eq!(hex::encode(public), s(v, "recipient_public_hex"));
                assert_eq!(
                    aster_grant_seal_to_key(public.as_ptr(), &c, b(&plaintext), &mut env),
                    OK
                );
                let env = take(env);
                assert_eq!(
                    aster_grant_open_with_key(key.as_ptr(), &c, b(&env), &mut opened),
                    OK
                );
            }
            assert_eq!(take(opened), plaintext, "{name} reseal");
        }
    }
    for v in doc["open_invalid"].as_array().unwrap() {
        let ctx = Ctx::from_json(&v["context"]);
        let (rc, out) = unsafe { open_by_scheme(v, &ctx) };
        assert_eq!(rc, VERIFY, "{} opened", s(v, "name"));
        assert!(out.is_empty());
    }
}

#[test]
fn identity_scheme_matches_explicit_key_and_hpke() {
    // seal_grant == seal to the node-id-derived X25519 key; open_grant ==
    // open with the identity-derived X25519 secret; and a grant envelope is
    // a plain HPKE envelope under the grant AAD.
    let doc = vectors();
    let v = &doc["open"][0];
    assert_eq!(s(v, "scheme"), "identity");
    let ctx = Ctx::from_json(&v["context"]);
    let c = ctx.c();
    let identity = h(v, "recipient_key_hex");
    let env = h(v, "envelope_hex");
    unsafe {
        let mut x_secret = [0u8; 32];
        assert_eq!(
            aster_hpke_x25519_secret_from_identity(identity.as_ptr(), x_secret.as_mut_ptr()),
            OK
        );
        let mut out = empty();
        assert_eq!(
            aster_grant_open_with_key(x_secret.as_ptr(), &c, b(&env), &mut out),
            OK
        );
        assert_eq!(hex::encode(take(out)), s(v, "plaintext_hex"));

        let mut aad = empty();
        assert_eq!(aster_grant_context_aad(&c, &mut aad), OK);
        let aad = take(aad);
        assert_eq!(
            aster_hpke_open(x_secret.as_ptr(), b(&aad), b(&env), &mut out),
            OK
        );
        assert_eq!(hex::encode(take(out)), s(v, "plaintext_hex"));

        let (mut from_node_id, mut from_private, mut from_identity) =
            ([0u8; 32], [0u8; 32], [0u8; 32]);
        assert_eq!(
            aster_hpke_x25519_public_from_node_id(
                b(ctx.recipient.as_bytes()),
                from_node_id.as_mut_ptr()
            ),
            OK
        );
        assert_eq!(
            aster_hpke_public_key_from_private(x_secret.as_ptr(), from_private.as_mut_ptr()),
            OK
        );
        let mut node_id = [0u8; 32];
        assert_eq!(
            aster_id_parse(
                aster_id_kind_t::ASTER_ID_NODE_ID as u32,
                b(ctx.recipient.as_bytes()),
                node_id.as_mut_ptr()
            ),
            OK
        );
        assert_eq!(
            aster_hpke_x25519_public_from_identity(node_id.as_ptr(), from_identity.as_mut_ptr()),
            OK
        );
        assert_eq!(from_node_id, from_private);
        assert_eq!(from_node_id, from_identity);
        // The recipient node id is the identity's public key.
        let mut public = [0u8; 32];
        assert_eq!(
            aster_secret_key_public_key(identity.as_ptr(), public.as_mut_ptr()),
            OK
        );
        assert_eq!(public, node_id);
    }
}

#[test]
fn namespace_vectors() {
    let doc = vectors();
    for v in doc["namespace_open"].as_array().unwrap() {
        let ctx = Ctx::from_json(&v["context"]);
        let c = ctx.c();
        let secret = h(v, "recipient_secret_hex");
        let env = h(v, "envelope_hex");
        unsafe {
            let (mut access, mut key) = (0u32, [0u8; 32]);
            assert_eq!(
                aster_namespace_grant_open(
                    secret.as_ptr(),
                    &c,
                    b(&env),
                    &mut access,
                    key.as_mut_ptr()
                ),
                OK
            );
            let can_write = access == aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE as u32;
            assert_eq!(can_write, v["can_write"].as_bool().unwrap());
            let mut enc = empty();
            assert_eq!(
                aster_namespace_capability_encode(access, key.as_ptr(), &mut enc),
                OK
            );
            assert_eq!(hex::encode(take(enc)), s(v, "capability_hex"));
            // And decode of the vector bytes agrees.
            let cap_bytes = h(v, "capability_hex");
            let (mut access2, mut key2) = (0u32, [0u8; 32]);
            assert_eq!(
                aster_namespace_capability_decode(b(&cap_bytes), &mut access2, key2.as_mut_ptr()),
                OK
            );
            assert_eq!((access2, key2), (access, key));

            // Reseals under the same rule.
            let mut env2 = empty();
            assert_eq!(
                aster_namespace_grant_seal(&c, access, key.as_ptr(), &mut env2),
                OK
            );
            let env2 = take(env2);
            let (mut access3, mut key3) = (0u32, [0u8; 32]);
            assert_eq!(
                aster_namespace_grant_open(
                    secret.as_ptr(),
                    &c,
                    b(&env2),
                    &mut access3,
                    key3.as_mut_ptr()
                ),
                OK
            );
            assert_eq!((access3, key3), (access, key));

            // The opposite access is refused by the role rule at seal time.
            let flipped = if can_write {
                aster_local_access_t::ASTER_LOCAL_ACCESS_READ
            } else {
                aster_local_access_t::ASTER_LOCAL_ACCESS_WRITE
            } as u32;
            let mut none = empty();
            if ctx.role == "read" || ctx.role == "write" {
                assert_eq!(
                    aster_namespace_grant_seal(&c, flipped, key.as_ptr(), &mut none),
                    VERIFY
                );
                assert!(none.ptr.is_null());
            }
            assert_eq!(
                aster_namespace_grant_seal(&c, 9, key.as_ptr(), &mut none),
                INVALID
            );
        }
    }
    for v in doc["namespace_open_invalid"].as_array().unwrap() {
        let ctx = Ctx::from_json(&v["context"]);
        let c = ctx.c();
        let secret = h(v, "recipient_secret_hex");
        let env = h(v, "envelope_hex");
        unsafe {
            // Opens as a raw grant ...
            let mut raw = empty();
            assert_eq!(aster_grant_open(secret.as_ptr(), &c, b(&env), &mut raw), OK);
            take(raw);
            // ... but the namespace open re-checks the role rule.
            let (mut access, mut key) = (0u32, [0u8; 32]);
            assert_eq!(
                aster_namespace_grant_open(
                    secret.as_ptr(),
                    &c,
                    b(&env),
                    &mut access,
                    key.as_mut_ptr()
                ),
                VERIFY
            );
        }
    }
    for v in doc["role_rule"].as_array().unwrap() {
        let can_write = v["can_write"].as_bool().unwrap() as u32;
        let rc = unsafe { aster_grant_check_role(b(s(v, "role").as_bytes()), can_write) };
        let want = if v["ok"].as_bool().unwrap() {
            OK
        } else {
            VERIFY
        };
        assert_eq!(rc, want, "role {:?} can_write {can_write}", s(v, "role"));
    }
}

#[test]
fn reconcile_vectors() {
    use aster_grant_outcome_kind_t::*;
    let doc = vectors();
    for v in doc["reconcile"].as_array().unwrap() {
        let name = s(v, "name");
        let ctx = Ctx::from_json(&v["context"]);
        let c = ctx.c();
        let secret = h(v, "recipient_secret_hex");
        let record = v["record_hex"].as_str().map(|x| hex::decode(x).unwrap());
        let record_b = record.as_deref().map(b);
        let record_ptr = record_b
            .as_ref()
            .map_or(ptr::null(), |r| r as *const aster_bytes_t);
        let expect = &v["outcome"];
        let namespace = match s(v, "payload") {
            "raw" => false,
            "namespace" => true,
            other => panic!("unknown payload {other}"),
        };
        unsafe {
            let mut o: aster_grant_outcome_t = std::mem::zeroed();
            let rc = if namespace {
                aster_namespace_grant_reconcile(secret.as_ptr(), &c, record_ptr, &mut o)
            } else {
                aster_grant_reconcile(secret.as_ptr(), &c, record_ptr, &mut o)
            };
            assert_eq!(rc, OK, "{name}");
            assert_eq!(
                o.struct_size as usize,
                std::mem::size_of::<aster_grant_outcome_t>()
            );
            let kind = match o.kind {
                x if x == ASTER_GRANT_OUTCOME_ACTIVE as u32 => "active",
                x if x == ASTER_GRANT_OUTCOME_REVOKED as u32 => "revoked",
                x if x == ASTER_GRANT_OUTCOME_UNOPENABLE as u32 => "unopenable",
                x if x == ASTER_GRANT_OUTCOME_ABSENT as u32 => "absent",
                x => panic!("{name}: unknown kind {x}"),
            };
            assert_eq!(kind, s(expect, "kind"), "{name}");
            assert_eq!(
                o.drops_capability,
                matches!(kind, "revoked" | "unopenable") as u32,
                "{name}"
            );
            match kind {
                "active" if namespace => {
                    let key = view(o.payload);
                    let mut enc = empty();
                    assert_eq!(
                        aster_namespace_capability_encode(o.access, key.as_ptr(), &mut enc),
                        OK
                    );
                    assert_eq!(
                        hex::encode(take(enc)),
                        s(expect, "capability_hex"),
                        "{name}"
                    );
                }
                "active" => {
                    assert_eq!(o.access, 0);
                    assert_eq!(
                        hex::encode(view(o.payload)),
                        s(expect, "plaintext_hex"),
                        "{name}"
                    );
                }
                "revoked" => {
                    assert_eq!(
                        String::from_utf8(view(o.reason)).unwrap(),
                        s(expect, "reason")
                    );
                    assert_eq!(String::from_utf8(view(o.role)).unwrap(), s(expect, "role"));
                }
                "unopenable" => {
                    let msg = String::from_utf8(view(o.reason)).unwrap();
                    assert!(!msg.is_empty(), "{name}");
                    if let Some(needle) = expect["error_contains"].as_str() {
                        assert!(msg.contains(needle), "{name}: {msg}");
                    }
                }
                _ => assert!(o.storage.is_null(), "{name}"),
            }
            assert_eq!(aster_grant_outcome_release(&mut o), OK);
            assert!(o.storage.is_null() && o.payload.ptr.is_null());
            assert_eq!(aster_grant_outcome_release(&mut o), OK);
        }
    }
}

#[test]
fn grant_record_roundtrip_and_vectors() {
    use aster_grant_state_t::*;
    let doc = vectors();
    unsafe {
        // Every present reconcile record either decodes, or is refused as malformed.
        let mut decoded_some = false;
        for v in doc["reconcile"].as_array().unwrap() {
            let Some(hexrec) = v["record_hex"].as_str() else {
                continue;
            };
            let bytes = hex::decode(hexrec).unwrap();
            let mut r: aster_grant_record_t = std::mem::zeroed();
            let rc = aster_grant_record_decode(b(&bytes), &mut r);
            if s(v, "name") == "garbage-record" {
                assert_eq!(rc, INVALID);
                assert!(r.storage.is_null());
                continue;
            }
            assert_eq!(rc, OK, "{}", s(v, "name"));
            decoded_some = true;
            // Re-encoding reproduces the record bytes exactly.
            let mut enc = empty();
            assert_eq!(
                aster_grant_record_encode(&r, &mut enc),
                OK,
                "{}",
                s(v, "name")
            );
            assert_eq!(take(enc), bytes, "{}", s(v, "name"));
            if s(&v["outcome"], "kind") == "revoked" {
                assert_eq!(r.state, ASTER_GRANT_STATE_REVOKED as u32);
                assert_eq!(
                    String::from_utf8(view(r.reason)).unwrap(),
                    s(&v["outcome"], "reason")
                );
            }
            assert_eq!(aster_grant_record_release(&mut r), OK);
            assert_eq!(aster_grant_record_release(&mut r), OK);
        }
        assert!(decoded_some);

        // Build a fresh active record around a sealed grant; reconcile opens it.
        let v = &doc["open"][0];
        let ctx = Ctx::from_json(&v["context"]);
        let c = ctx.c();
        let mut env = empty();
        assert_eq!(aster_grant_seal(&c, b(b"payload"), &mut env), OK);
        let env = take(env);
        let rec = aster_grant_record_t {
            struct_size: std::mem::size_of::<aster_grant_record_t>() as u32,
            state: ASTER_GRANT_STATE_ACTIVE as u32,
            context_version: 2,
            reserved: 0,
            role: b(ctx.role.as_bytes()),
            envelope: b(&env),
            reason: empty(),
            storage: ptr::null_mut(),
        };
        let mut enc = empty();
        assert_eq!(aster_grant_record_encode(&rec, &mut enc), OK);
        let enc = take(enc);
        let mut back: aster_grant_record_t = std::mem::zeroed();
        assert_eq!(aster_grant_record_decode(b(&enc), &mut back), OK);
        assert_eq!(back.context_version, 2);
        assert_eq!(view(back.envelope), env);
        aster_grant_record_release(&mut back);
        let mut o: aster_grant_outcome_t = std::mem::zeroed();
        let secret = h(v, "recipient_key_hex");
        let rb = b(&enc);
        assert_eq!(aster_grant_reconcile(secret.as_ptr(), &c, &rb, &mut o), OK);
        assert_eq!(
            o.kind,
            aster_grant_outcome_kind_t::ASTER_GRANT_OUTCOME_ACTIVE as u32
        );
        assert_eq!(view(o.payload), b"payload");
        aster_grant_outcome_release(&mut o);

        // Invalid shapes are refused.
        let mut none = empty();
        let no_env = aster_grant_record_t {
            envelope: empty(),
            ..rec
        };
        assert_eq!(aster_grant_record_encode(&no_env, &mut none), INVALID);
        let revoked_with_env = aster_grant_record_t {
            state: ASTER_GRANT_STATE_REVOKED as u32,
            ..rec
        };
        assert_eq!(
            aster_grant_record_encode(&revoked_with_env, &mut none),
            INVALID
        );
        let bad_state = aster_grant_record_t { state: 7, ..rec };
        assert_eq!(aster_grant_record_encode(&bad_state, &mut none), INVALID);
        assert!(none.ptr.is_null());
    }
}
