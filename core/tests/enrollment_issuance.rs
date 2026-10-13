#![cfg(feature = "enrollment")]

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use aster_transport_core::enrollment::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

const NOW: u64 = 1_800_000_000;

fn subject() -> [u8; 32] {
    SigningKey::from_bytes(&[11; 32]).verifying_key().to_bytes()
}

fn issuer() -> JwtEnrollmentIssuer {
    JwtEnrollmentIssuer::from_seed(&[10; 32])
}

fn request() -> EnrollmentIssueRequest {
    let mut request = EnrollmentIssueRequest::new(subject(), "build-fleet");
    request.claims.scopes = Some(BTreeSet::from(["build-worker".into()]));
    request.claims.tags = Some(BTreeSet::new());
    request.claims.attributes = Some(BTreeMap::from([
        ("count".into(), TypedAttribute::Int(i64::MAX)),
        ("capacity".into(), TypedAttribute::Float(1.25)),
    ]));
    request
}

fn sign_raw(payload: &str) -> String {
    let header = json!({"alg": "EdDSA", "typ": ENROLLMENT_JWT_TYPE});
    let message = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(payload)
    );
    format!(
        "{}.{}",
        message,
        URL_SAFE_NO_PAD.encode(
            SigningKey::from_bytes(&[10; 32])
                .sign(message.as_bytes())
                .to_bytes()
        )
    )
}

fn issued_payload() -> Value {
    let artifact = issuer().issue(&request(), NOW).unwrap();
    let payload = artifact.jwt().split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

#[test]
fn authorized_issuer_derives_profile_and_generates_unique_ids() {
    let issuer = issuer();
    let verifier = JwtEnrollmentVerifier::new(issuer.public_key(), "build-fleet").unwrap();
    assert_eq!(verifier.trusted_root(), &issuer.public_key());
    assert_eq!(verifier.intended_audience(), "build-fleet");
    let request = request();
    let mut ids = BTreeSet::new();
    for _ in 0..16 {
        let artifact = issuer.issue(&request, NOW).unwrap();
        assert!(ids.insert(artifact.credential_id().to_owned()));
        assert_eq!(
            URL_SAFE_NO_PAD
                .decode(artifact.credential_id())
                .unwrap()
                .len(),
            32
        );
        let verified = verifier.verify(artifact.jwt(), &subject(), NOW).unwrap();
        assert_eq!(verified.issuer(), &issuer.public_key());
        assert_eq!(verified.subject(), &subject());
        assert_eq!(verified.credential_id(), Some(artifact.credential_id()));
        assert_eq!(verified.original_jwt(), Some(artifact.jwt()));
        assert_eq!(verified.claims(), &request.claims);
        assert_eq!(verified.expires_at(), None);
        assert!(verifier
            .verify(artifact.jwt(), &subject(), u64::MAX)
            .is_ok());
    }
}

#[test]
fn issuance_supports_requested_id_multiple_audiences_and_future_activation() {
    let issuer = issuer();
    let mut request = request();
    request.credential_id = Some("root-selected-id".into());
    request.audience = Audience::Many(vec!["build-fleet".into(), "urn:aster:fleet:workers".into()]);
    request.not_before = Some(NOW + 60);
    request.expires_at = Some(NOW + 120);
    let artifact = issuer.issue(&request, NOW).unwrap();
    assert_eq!(artifact.credential_id(), "root-selected-id");
    for audience in ["build-fleet", "urn:aster:fleet:workers"] {
        let verifier = JwtEnrollmentVerifier::new(issuer.public_key(), audience).unwrap();
        // The default 60 s activation leeway admits from NOW; earlier is skew.
        assert!(verifier.verify(artifact.jwt(), &subject(), NOW).is_ok());
        assert_eq!(
            verifier
                .verify(artifact.jwt(), &subject(), NOW - 1)
                .unwrap_err(),
            EnrollmentError::ClockSkew { offset_seconds: 61 }
        );
        assert!(verifier
            .verify(artifact.jwt(), &subject(), NOW + 60)
            .is_ok());
        assert_eq!(
            verifier
                .verify(artifact.jwt(), &subject(), NOW + 60)
                .unwrap()
                .not_before(),
            Some(NOW + 60)
        );
        assert_eq!(
            verifier
                .verify(artifact.jwt(), &subject(), NOW + 120)
                .unwrap_err(),
            EnrollmentError::InvalidValidity
        );
    }
}

#[test]
fn issuer_rejects_invalid_grants_before_signing() {
    let issuer = issuer();
    let mut invalid = request();
    invalid.expires_at = Some(NOW);
    assert_eq!(
        issuer.issue(&invalid, NOW).unwrap_err(),
        EnrollmentError::InvalidValidity
    );
    invalid.expires_at = Some(NOW + 1);
    invalid.not_before = Some(NOW + 1);
    assert_eq!(
        issuer.issue(&invalid, NOW).unwrap_err(),
        EnrollmentError::InvalidValidity
    );
    for id in [String::new(), "x".repeat(257), "unsafe\nidentifier".into()] {
        let mut invalid = request();
        invalid.credential_id = Some(id);
        assert_eq!(
            issuer.issue(&invalid, NOW).unwrap_err(),
            EnrollmentError::InvalidClaims
        );
    }
    for float in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut invalid = request();
        invalid
            .claims
            .attributes
            .as_mut()
            .unwrap()
            .insert("bad".into(), TypedAttribute::Float(float));
        assert_eq!(
            issuer.issue(&invalid, NOW).unwrap_err(),
            EnrollmentError::InvalidClaims
        );
    }
    let mut invalid = request();
    invalid.subject = [0; 32];
    assert_eq!(
        issuer.issue(&invalid, NOW).unwrap_err(),
        EnrollmentError::InvalidIdentity
    );
    let mut huge = request();
    huge.claims.tags = Some(BTreeSet::from(["a".repeat(MAX_JWT_BYTES)]));
    assert_eq!(
        issuer.issue(&huge, NOW).unwrap_err(),
        EnrollmentError::TokenTooLarge
    );
}

#[test]
fn signed_profile_rejects_weak_keys_and_malformed_string_or_uri_audiences() {
    for weak in [[0; 32], {
        let mut key = [0; 32];
        key[0] = 1;
        key
    }] {
        assert!(JwtEnrollmentVerifier::new(weak, "build-fleet").is_err());
    }
    let root = issuer().public_key();
    for audience in [
        "",
        "bad\naudience",
        "1:bad-scheme",
        "https://example.org/bad path",
        "https://example.org/%ZZ",
        "https://example.org/\\path",
        "urn:é",
    ] {
        assert!(
            JwtEnrollmentVerifier::new(root, audience).is_err(),
            "{audience}"
        );
        let mut invalid = request();
        invalid.audience = Audience::One(audience.into());
        assert_eq!(
            issuer().issue(&invalid, NOW).unwrap_err(),
            EnrollmentError::InvalidClaims
        );
    }
    for audience in [
        "build fleet",
        "équipe",
        "urn:aster:fleet",
        "https://example.org/build%20fleet",
    ] {
        assert!(
            JwtEnrollmentVerifier::new(root, audience).is_ok(),
            "{audience}"
        );
    }
    // Matching one valid audience cannot hide a malformed second audience.
    let mut payload = issued_payload();
    payload["aud"] = json!(["build-fleet", "bad: invalid uri"]);
    assert_eq!(
        JwtEnrollmentVerifier::new(root, "build-fleet")
            .unwrap()
            .verify(&sign_raw(&payload.to_string()), &subject(), NOW)
            .unwrap_err(),
        EnrollmentError::InvalidClaims
    );
}

#[test]
fn duplicate_fields_are_rejected_even_in_ignored_nested_claims() {
    let verifier = JwtEnrollmentVerifier::new(issuer().public_key(), "build-fleet").unwrap();
    let payload = issued_payload().to_string();
    let optional_duplicate = format!(
        "{},\"https://example.org/optional\":{{\"duplicate\":true,\"duplicate\":false}}}}",
        &payload[..payload.len() - 1]
    );
    assert!(verifier
        .verify(&sign_raw(&optional_duplicate), &subject(), NOW)
        .is_err());
    let duplicate_registered = format!("{},\"jti\":\"other-id\"}}", &payload[..payload.len() - 1]);
    assert!(verifier
        .verify(&sign_raw(&duplicate_registered), &subject(), NOW)
        .is_err());
    let mut p = issued_payload();
    p["aster_enrollment_v1"]["attributes"] = json!({});
    let p = p.to_string().replace("\"attributes\":{}", "\"attributes\":{\"role\":{\"type\":\"string\",\"value\":\"one\"},\"role\":{\"type\":\"string\",\"value\":\"two\"}}");
    assert!(verifier.verify(&sign_raw(&p), &subject(), NOW).is_err());
    // Deep optional claims are still subject to serde_json's recursion bound.
    let deep = format!(
        "{},\"optional\":{}0{}}}",
        &payload[..payload.len() - 1],
        "[".repeat(150),
        "]".repeat(150)
    );
    assert!(verifier.verify(&sign_raw(&deep), &subject(), NOW).is_err());
}

#[test]
fn compact_jwt_rejects_padding_empty_extra_and_noncanonical_segments() {
    let issuer = issuer();
    let token = issuer.issue(&request(), NOW).unwrap().into_jwt();
    let verifier = JwtEnrollmentVerifier::new(issuer.public_key(), "build-fleet").unwrap();
    let parts: Vec<_> = token.split('.').collect();
    for changed in [
        format!("{}.{}=.{}", parts[0], parts[1], parts[2]),
        format!("{}.{}.{}=", parts[0], parts[1], parts[2]),
        format!("{}.{}.{}.", parts[0], parts[1], parts[2]),
        format!(".{}.{}", parts[1], parts[2]),
        format!("{}..{}", parts[0], parts[2]),
        format!("{}.{}.", parts[0], parts[1]),
        format!("{}.{}.AA", parts[0], parts[1]),
    ] {
        assert!(verifier.verify(&changed, &subject(), NOW).is_err());
    }
}

#[tokio::test]
async fn previously_verified_evidence_cannot_outlive_its_signed_bounds() {
    let issuer = issuer();
    let mut request = request();
    request.expires_at = Some(NOW + 1);
    let artifact = issuer.issue(&request, NOW).unwrap();
    let verifier = JwtEnrollmentVerifier::new(issuer.public_key(), "build-fleet").unwrap();
    let evidence = verifier.verify(artifact.jwt(), &subject(), NOW).unwrap();
    let policy = PolicyExpressionV1::Predicate(PolicyPredicate::TolerateStalePolicy);
    let result = admit(
        &evidence,
        &KnownRevocations::default(),
        &AdmissionContext::default(),
        &policy,
        Duration::from_secs(1),
        || NOW + 1,
    )
    .await;
    assert_eq!(
        result,
        AdmissionDecision::Deny(DenialReason::InvalidValidity)
    );
}

#[test]
fn attribute_file_parser_preserves_all_types_without_coercion() {
    let attributes = AdmissionClaims::parse_attributes_json(
        br#"{
            "active":{"type":"bool","value":true},
            "count":{"type":"int","value":9223372036854775807},
            "capacity":{"type":"float","value":1.25},
            "region":{"type":"string","value":"eu"}
        }"#,
    )
    .unwrap();
    assert_eq!(
        attributes,
        BTreeMap::from([
            ("active".into(), TypedAttribute::Bool(true)),
            ("count".into(), TypedAttribute::Int(i64::MAX)),
            ("capacity".into(), TypedAttribute::Float(1.25)),
            ("region".into(), TypedAttribute::String("eu".into())),
        ])
    );
    assert!(AdmissionClaims::parse_attributes_json(b"{}")
        .unwrap()
        .is_empty());
}

#[test]
fn attribute_file_parser_rejects_ambiguous_invalid_and_oversized_inputs() {
    for input in [
        r#"{"role":{"type":"string","value":"viewer"},"role":{"type":"string","value":"admin"}}"#,
        r#"{"role":{"type":"string","value":"viewer","value":"admin"}}"#,
        // Escaped spelling is still the same decoded JSON property name.
        r#"{"role":{"type":"string","value":"viewer"},"\u0072ole":{"type":"string","value":"admin"}}"#,
        r#"{"role":{"type":"string","value":"admin","mandatory":true}}"#,
        r#"{"role":{"type":"unknown","value":"admin"}}"#,
        r#"{"role":{"type":"bool","value":"true"}}"#,
        r#"{"role":{"type":"int","value":1.25}}"#,
        r#"{"role":{"type":"int","value":9223372036854775808}}"#,
        r#"{"role":{"type":"float","value":1e999}}"#,
        "null",
        "[]",
    ] {
        assert_eq!(
            AdmissionClaims::parse_attributes_json(input.as_bytes()).unwrap_err(),
            EnrollmentError::InvalidClaims,
            "{input}"
        );
    }
    assert_eq!(
        AdmissionClaims::parse_attributes_json(&vec![b' '; MAX_JWT_BYTES + 1]).unwrap_err(),
        EnrollmentError::TokenTooLarge
    );
    // The same strict typed grammar applies to signed credentials, regardless
    // of whether they were produced through the attribute-file API.
    let mut payload = issued_payload();
    payload["aster_enrollment_v1"]["attributes"]["count"]["extra"] = json!(true);
    assert!(
        JwtEnrollmentVerifier::new(issuer().public_key(), "build-fleet")
            .unwrap()
            .verify(&sign_raw(&payload.to_string()), &subject(), NOW)
            .is_err()
    );
}
