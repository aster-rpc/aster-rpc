//! Stateless C provisioning exercised with an external Ed25519 signer.
use aster_transport_ffi::{provisioning::*, *};
use serde_json::{json, Value};
use std::{ptr, slice};

fn span(value: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: value.as_ptr(),
        len: value.len(),
    }
}
fn invoke(call: impl FnOnce(*mut aster_provisioning_result_t) -> i32) -> (i32, Vec<u8>, Vec<u8>) {
    unsafe {
        let mut out: aster_provisioning_result_t = std::mem::zeroed();
        let status = call(&mut out);
        let copy = |span: aster_bytes_t| {
            if span.ptr.is_null() {
                assert_eq!(span.len, 0);
                Vec::new()
            } else {
                let result = slice::from_raw_parts(span.ptr, span.len).to_vec();
                assert_eq!(aster_string_release(span.ptr, span.len), 0);
                result
            }
        };
        (status, copy(out.data), copy(out.error))
    }
}
fn ok(call: impl FnOnce(*mut aster_provisioning_result_t) -> i32) -> Vec<u8> {
    let (status, data, error) = invoke(call);
    assert_eq!(status, 0, "{}", String::from_utf8_lossy(&error));
    assert!(error.is_empty());
    data
}
fn error(code: &str, call: impl FnOnce(*mut aster_provisioning_result_t) -> i32) {
    let (status, data, error) = invoke(call);
    assert_ne!(status, 0);
    assert!(data.is_empty());
    let value: Value = serde_json::from_slice(&error).unwrap();
    assert_eq!(value["code"], code);
    assert!(!value["detail"].as_str().unwrap().is_empty());
}
fn fixture() -> (iroh::SecretKey, iroh::SecretKey, Value) {
    let root = iroh::SecretKey::from_bytes(&[7; 32]);
    let voter = iroh::SecretKey::from_bytes(&[8; 32]);
    let id = voter.public();
    let ticket = ok(|out| unsafe {
        aster_coordination_canonical_candidate_ticket(span(id.as_bytes()), out)
    });
    let doc = json!({
        "group_id": "11".repeat(32), "authority_domain": "test.aster", "genesis_nonce": "22".repeat(32),
        "deployment_profile_code": 2,
        "initial_voters": [{"node_id": id.to_string(), "ticket": String::from_utf8(ticket).unwrap()}],
        "initial_admins": [id.to_string()], "recovery_export_hash": "33".repeat(32)
    });
    (root, voter, doc)
}
fn encode_sign(root: &iroh::SecretKey, doc: &Value, join: bool) -> (Vec<u8>, Vec<u8>) {
    let input = doc.to_string();
    let document = ok(|out| unsafe {
        if join {
            aster_coordination_canonical_join_permit(span(input.as_bytes()), out)
        } else {
            aster_coordination_canonical_genesis(span(input.as_bytes()), out)
        }
    });
    let preimage = ok(|out| unsafe {
        if join {
            aster_coordination_join_signing_bytes(span(&document), out)
        } else {
            aster_coordination_genesis_signing_bytes(span(&document), out)
        }
    });
    let signature = root.sign(&preimage).to_bytes();
    let envelope = ok(|out| unsafe {
        aster_coordination_signed_document_from_parts(
            span(&document),
            span(root.public().as_bytes()),
            span(&signature),
            out,
        )
    });
    (document, envelope)
}
#[test]
fn external_signer_genesis_and_join_verify_against_explicit_pins() {
    let (root, voter, input) = fixture();
    let (document, envelope) = encode_sign(&root, &input, false);
    assert_eq!(
        document,
        aster::binding_support::provisioning::canonical_genesis_json(&input.to_string()).unwrap()
    );
    let hash = ok(|out| unsafe { aster_coordination_genesis_hash(span(&document), out) });
    assert_eq!(hash.len(), 32);
    let verified = ok(|out| unsafe {
        aster_coordination_verify_genesis(
            span(root.public().as_bytes()),
            span(b"test.aster"),
            span(&envelope),
            out,
        )
    });
    let verified: Value = serde_json::from_slice(&verified).unwrap();
    assert_eq!(verified["document_bytes"], hex::encode(&document));
    assert_eq!(verified["genesis_hash"], hex::encode(&hash));
    assert_eq!(verified["authority_public_key"], root.public().to_string());
    assert_eq!(verified["group_id"], input["group_id"]);
    assert_eq!(
        verified["recovery_export_hash"],
        input["recovery_export_hash"]
    );
    error("wrong_domain", |out| unsafe {
        aster_coordination_verify_genesis(
            span(root.public().as_bytes()),
            span(b"other.aster"),
            span(&envelope),
            out,
        )
    });
    error("wrong_authority_key", |out| unsafe {
        aster_coordination_verify_genesis(
            span(voter.public().as_bytes()),
            span(b"test.aster"),
            span(&envelope),
            out,
        )
    });
    let bad = ok(|out| unsafe {
        aster_coordination_signed_document_from_parts(
            span(&document),
            span(root.public().as_bytes()),
            span(&[0; 64]),
            out,
        )
    });
    error("bad_signature", |out| unsafe {
        aster_coordination_verify_genesis(
            span(root.public().as_bytes()),
            span(b"test.aster"),
            span(&bad),
            out,
        )
    });
    let candidate = iroh::SecretKey::from_bytes(&[9; 32]);
    let join = json!({"group_id":input["group_id"],"authority_domain":"test.aster", "genesis_hash":hex::encode(hash),"candidate_node_id":candidate.public().to_string(),"bootstrap_senders":[voter.public().to_string()],"join_nonce":"44".repeat(32)});
    let (join_document, permit) = encode_sign(&root, &join, true);
    let verified = ok(|out| unsafe {
        aster_coordination_verify_join_permit_for_start(
            span(root.public().as_bytes()),
            span(b"test.aster"),
            span(candidate.public().as_bytes()),
            span(&permit),
            out,
        )
    });
    let verified: Value = serde_json::from_slice(&verified).unwrap();
    assert_eq!(verified["document_bytes"], hex::encode(join_document));
    assert_eq!(
        verified["candidate_node_id"],
        candidate.public().to_string()
    );
    error("wrong_node", |out| unsafe {
        aster_coordination_verify_join_permit_for_start(
            span(root.public().as_bytes()),
            span(b"test.aster"),
            span(voter.public().as_bytes()),
            span(&permit),
            out,
        )
    });
}
#[test]
fn canonical_encoding_never_silently_repairs_policy_and_domains_are_separate() {
    let (root, _, mut input) = fixture();
    input["deployment_profile_code"] = json!(123);
    let (document, envelope) = encode_sign(&root, &input, false);
    let genesis =
        ok(|out| unsafe { aster_coordination_genesis_signing_bytes(span(&document), out) });
    let join = ok(|out| unsafe { aster_coordination_join_signing_bytes(span(&document), out) });
    assert_ne!(genesis, join);
    error("unknown_profile", |out| unsafe {
        aster_coordination_verify_genesis(
            span(root.public().as_bytes()),
            span(b"test.aster"),
            span(&envelope),
            out,
        )
    });
    input["unexpected"] = json!(true);
    error("invalid_input", |out| unsafe {
        aster_coordination_canonical_genesis(span(input.to_string().as_bytes()), out)
    });
}
#[test]
fn invalid_ffi_inputs_return_owned_errors_and_never_unwind() {
    error("invalid_input", |out| unsafe {
        aster_coordination_canonical_genesis(
            aster_bytes_t {
                ptr: ptr::null(),
                len: 1,
            },
            out,
        )
    });
    error("invalid_input", |out| unsafe {
        aster_coordination_canonical_genesis(span(&[255]), out)
    });
    error("invalid_input", |out| unsafe {
        aster_coordination_canonical_candidate_ticket(span(&[1; 31]), out)
    });
    error("invalid_input", |out| unsafe {
        aster_coordination_signed_document_from_parts(
            span(b"x"),
            span(&[1; 32]),
            span(&[2; 63]),
            out,
        )
    });
    error("malformed", |out| unsafe {
        aster_coordination_verify_genesis(
            span(&[1; 32]),
            span(b"test.aster"),
            span(b"not an envelope"),
            out,
        )
    });
    assert_eq!(
        unsafe { aster_coordination_genesis_hash(span(b"x"), ptr::null_mut()) },
        aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32
    );
}
