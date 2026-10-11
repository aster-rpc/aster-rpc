//! Portable provisioning must reproduce the Rust consumer's exact Fory bytes.
#![cfg(all(feature = "binding-support", feature = "coordination-client"))]
use aster::binding_support::provisioning as portable;
use aster::coordination::model::authority as native;
use aster::coordination::raft::{GenesisV1, JoinPermitV1, NodeIdV1, VoterSeedV1};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

const DOMAIN: &str = "provisioning.test";
fn root() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
fn node(seed: u8) -> [u8; 32] {
    SigningKey::from_bytes(&[seed; 32])
        .verifying_key()
        .to_bytes()
}
fn genesis_input() -> Value {
    json!({
        "group_id": hex::encode([21; 32]), "authority_domain": DOMAIN,
        "genesis_nonce": hex::encode([22; 32]), "deployment_profile_code": 2,
        "initial_voters": [{"node_id": hex::encode(node(11)), "ticket": native::canonical_candidate_ticket(&node(11)).unwrap()}],
        "initial_admins": [hex::encode(node(12))], "recovery_export_hash": hex::encode([24; 32]),
    })
}
fn genesis_document() -> Vec<u8> {
    portable::canonical_genesis_json(&genesis_input().to_string()).unwrap()
}
fn join_input() -> Value {
    json!({
        "group_id": hex::encode([21; 32]), "authority_domain": DOMAIN,
        "genesis_hash": hex::encode(native::genesis_hash(&genesis_document())),
        "candidate_node_id": hex::encode(node(13)),
        "bootstrap_senders": [hex::encode(node(11))], "join_nonce": hex::encode([23; 32]),
    })
}
fn signed(document: &[u8], genesis: bool) -> Vec<u8> {
    let preimage = if genesis {
        native::genesis_signing_bytes(document)
    } else {
        native::join_signing_bytes(document)
    };
    portable::signed_document_from_parts(
        document,
        &root().verifying_key().to_bytes(),
        &root().sign(&preimage).to_bytes(),
    )
    .unwrap()
}

#[test]
fn conformance_vectors_match_direct_rust_consumer_and_external_signer() {
    let genesis = genesis_document();
    let direct = GenesisV1 {
        format_version: 1,
        group_id: vec![21; 32],
        authority_domain: DOMAIN.into(),
        genesis_nonce: vec![22; 32],
        deployment_profile_code: 2,
        initial_voters: vec![VoterSeedV1 {
            node_id: node(11).to_vec(),
            ticket: native::canonical_candidate_ticket(&node(11)).unwrap(),
        }],
        initial_admins: vec![NodeIdV1 {
            node_id: node(12).to_vec(),
        }],
        recovery_export_hash: Some(vec![24; 32]),
    };
    assert_eq!(genesis, native::canonical_document_bytes(&direct).unwrap());
    let join = portable::canonical_join_permit_json(&join_input().to_string()).unwrap();
    let direct_join = JoinPermitV1 {
        format_version: 1,
        group_id: vec![21; 32],
        authority_domain: DOMAIN.into(),
        genesis_hash: native::genesis_hash(&genesis).to_vec(),
        candidate_node_id: node(13).to_vec(),
        candidate_ticket: native::canonical_candidate_ticket(&node(13)).unwrap(),
        bootstrap_senders: vec![NodeIdV1 {
            node_id: node(11).to_vec(),
        }],
        join_nonce: vec![23; 32],
    };
    assert_eq!(
        join,
        native::canonical_document_bytes(&direct_join).unwrap()
    );
    let key = root().verifying_key().to_bytes();
    let genesis_signed = signed(&genesis, true);
    let join_signed = signed(&join, false);
    let expected = json!({
        "format_version": 1, "test_only_authority_secret": hex::encode([7; 32]), "authority_public_key": hex::encode(key),
        "genesis": {
            "request": genesis_input(), "document_bytes": hex::encode(&genesis),
            "signing_bytes": hex::encode(portable::genesis_signing_bytes(&genesis)),
            "signature": hex::encode(root().sign(&native::genesis_signing_bytes(&genesis)).to_bytes()),
            "signed_document": hex::encode(&genesis_signed), "genesis_hash": hex::encode(portable::genesis_hash(&genesis)),
            "verified": serde_json::from_str::<Value>(&portable::verify_genesis_json(&key, DOMAIN, &genesis_signed).unwrap()).unwrap(),
        },
        "join": {
            "request": join_input(), "candidate_ticket": portable::canonical_candidate_ticket(&node(13)).unwrap(),
            "document_bytes": hex::encode(&join), "signing_bytes": hex::encode(portable::join_signing_bytes(&join)),
            "signature": hex::encode(root().sign(&native::join_signing_bytes(&join)).to_bytes()),
            "signed_document": hex::encode(&join_signed),
            "verified": serde_json::from_str::<Value>(&portable::verify_join_permit_for_start_json(&key, DOMAIN, &node(13), &join_signed).unwrap()).unwrap(),
        },
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/fixtures/coordination_provisioning_v1.json");
    if std::env::var_os("ASTER_UPDATE_PROVISIONING_VECTORS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&expected).unwrap()),
        )
        .unwrap();
    }
    let actual: Value =
        serde_json::from_slice(&std::fs::read(&path).expect("committed conformance vectors"))
            .unwrap();
    assert_eq!(actual, expected);
    // The external signer and native verifier are independent of the DTO.
    assert_eq!(
        native::verify_genesis(&key, DOMAIN, &genesis_signed)
            .unwrap()
            .document_bytes(),
        genesis
    );
    assert_eq!(
        native::verify_join_permit_for_start(&key, DOMAIN, &join_signed, &node(13))
            .unwrap()
            .document_bytes(),
        join
    );
}

#[test]
fn strict_json_fixed_fields_and_input_bounds_fail_without_protocol_panics() {
    for value in ["short".to_owned(), "g".repeat(64), "é".repeat(32)] {
        let mut input = genesis_input();
        input["group_id"] = json!(value);
        assert_eq!(
            portable::canonical_genesis_json(&input.to_string())
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }
    let mut unknown = genesis_input();
    unknown["format_version"] = json!(1);
    assert_eq!(
        portable::canonical_genesis_json(&unknown.to_string())
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    let mut join = join_input();
    join["candidate_ticket"] = json!("caller cannot inject a ticket");
    assert_eq!(
        portable::canonical_join_permit_json(&join.to_string())
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(
        portable::canonical_genesis_json(&" ".repeat(portable::MAX_PROVISIONING_INPUT_BYTES + 1))
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(
        portable::signed_document_from_parts(b"", &[0; 31], &[0; 64])
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(
        portable::signed_document_from_parts(b"", &[0; 32], &[0; 63])
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(
        portable::canonical_candidate_ticket(&[0; 31])
            .unwrap_err()
            .code(),
        "invalid_input"
    );
}

#[test]
fn verification_preserves_pin_domain_signature_and_native_numeric_errors() {
    let document = genesis_document();
    let envelope = signed(&document, true);
    let key = root().verifying_key().to_bytes();
    assert_eq!(
        portable::verify_genesis_json(&node(14), DOMAIN, &envelope)
            .unwrap_err()
            .code(),
        "wrong_authority_key"
    );
    assert_eq!(
        portable::verify_genesis_json(&key, "another.test", &envelope)
            .unwrap_err()
            .code(),
        "wrong_domain"
    );
    let wrong_domain_signature = signed(&document, false);
    assert_eq!(
        portable::verify_genesis_json(&key, DOMAIN, &wrong_domain_signature)
            .unwrap_err()
            .code(),
        "bad_signature"
    );
    assert_eq!(
        portable::verify_genesis_json(&key, DOMAIN, b"garbage")
            .unwrap_err()
            .code(),
        "malformed"
    );
    let mut invalid_profile = genesis_input();
    invalid_profile["deployment_profile_code"] = json!(99);
    let document = portable::canonical_genesis_json(&invalid_profile.to_string()).unwrap();
    let error = portable::verify_genesis_json(&key, DOMAIN, &signed(&document, true)).unwrap_err();
    assert_eq!(error.code(), "unknown_profile");
    assert_eq!(error.detail(), "UnknownProfile(99)");
}

#[test]
fn encoding_does_not_normalize_sets_or_grant_authority() {
    let key = root().verifying_key().to_bytes();
    let mut input = genesis_input();
    input["initial_admins"] = json!([hex::encode(key)]);
    let document = portable::canonical_genesis_json(&input.to_string()).unwrap();
    assert_eq!(
        portable::verify_genesis_json(&key, DOMAIN, &signed(&document, true))
            .unwrap_err()
            .code(),
        "root_is_not_delegable"
    );
    let mut input = genesis_input();
    input["initial_admins"] = json!([hex::encode(node(12)), hex::encode(node(12))]);
    let document = portable::canonical_genesis_json(&input.to_string()).unwrap();
    assert_eq!(
        portable::verify_genesis_json(&key, DOMAIN, &signed(&document, true))
            .unwrap_err()
            .code(),
        "initial_admins"
    );
    let mut input = genesis_input();
    input["initial_voters"][0]["ticket"] = json!("not a canonical ticket");
    let document = portable::canonical_genesis_json(&input.to_string()).unwrap();
    assert_eq!(
        portable::verify_genesis_json(&key, DOMAIN, &signed(&document, true))
            .unwrap_err()
            .code(),
        "voter_seed_ticket"
    );
}

#[test]
fn startup_permit_verifies_candidate_but_extracts_group_and_genesis() {
    let key = root().verifying_key().to_bytes();
    let mut input = join_input();
    input["group_id"] = json!(hex::encode([29; 32]));
    input["genesis_hash"] = json!(hex::encode([30; 32]));
    let document = portable::canonical_join_permit_json(&input.to_string()).unwrap();
    let envelope = signed(&document, false);
    let summary: Value = serde_json::from_str(
        &portable::verify_join_permit_for_start_json(&key, DOMAIN, &node(13), &envelope).unwrap(),
    )
    .unwrap();
    assert_eq!(summary["group_id"], input["group_id"]);
    assert_eq!(summary["genesis_hash"], input["genesis_hash"]);
    assert_eq!(
        portable::verify_join_permit_for_start_json(&key, DOMAIN, &node(14), &envelope)
            .unwrap_err()
            .code(),
        "wrong_node"
    );
    assert_eq!(
        portable::verify_join_permit_for_start_json(&key, "other.test", &node(13), &envelope)
            .unwrap_err()
            .code(),
        "wrong_domain"
    );
    input["bootstrap_senders"] = json!([hex::encode(node(11)), hex::encode(node(11))]);
    let document = portable::canonical_join_permit_json(&input.to_string()).unwrap();
    assert_eq!(
        portable::verify_join_permit_for_start_json(
            &key,
            DOMAIN,
            &node(13),
            &signed(&document, false)
        )
        .unwrap_err()
        .code(),
        "bootstrap_senders"
    );
}

#[test]
fn opaque_preimages_and_hash_do_not_assert_document_validity() {
    let arbitrary = b"not a Fory document\0\xff";
    assert_eq!(
        portable::genesis_signing_bytes(arbitrary),
        native::genesis_signing_bytes(arbitrary)
    );
    assert_eq!(
        portable::join_signing_bytes(arbitrary),
        native::join_signing_bytes(arbitrary)
    );
    assert_ne!(
        portable::genesis_signing_bytes(arbitrary),
        portable::join_signing_bytes(arbitrary)
    );
    assert_eq!(
        portable::genesis_hash(arbitrary),
        native::genesis_hash(arbitrary)
    );
}
