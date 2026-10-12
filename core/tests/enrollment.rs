#![cfg(feature = "enrollment")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aster_transport_core::enrollment::*;
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

const NOW: u64 = 1_800_000_000;

fn root() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}
fn peer() -> [u8; 32] {
    SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes()
}
fn claims() -> Value {
    json!({
        "iss": hex::encode(root().verifying_key().to_bytes()),
        "sub": hex::encode(peer()),
        "aud": "build-fleet",
        "jti": "credential-1",
        "iat": NOW - 60,
        "cnf": { "jwk": { "kty": "OKP", "crv": "Ed25519", "x": URL_SAFE_NO_PAD.encode(peer()) } },
        "aster_enrollment_v1": {
            "scopes": ["build-worker"], "tags": [], "attributes": {
                "enabled": { "type": "bool", "value": true },
                "workers": { "type": "int", "value": 8 },
                "load": { "type": "float", "value": 0.25 },
                "role": { "type": "string", "value": "builder" }
            }, "mandatory": []
        }
    })
}
fn header() -> Value {
    json!({"alg": "EdDSA", "typ": ENROLLMENT_JWT_TYPE})
}
fn sign_raw(header: &[u8], claims: &[u8], key: &SigningKey) -> String {
    let message = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    format!(
        "{}.{}",
        message,
        URL_SAFE_NO_PAD.encode(key.sign(message.as_bytes()).to_bytes())
    )
}
fn sign(header: &Value, claims: &Value, key: &SigningKey) -> String {
    sign_raw(
        &serde_json::to_vec(header).unwrap(),
        &serde_json::to_vec(claims).unwrap(),
        key,
    )
}
fn verifier() -> JwtEnrollmentVerifier {
    JwtEnrollmentVerifier::new(root().verifying_key().to_bytes(), "build-fleet").unwrap()
}
fn evidence(claims: &Value) -> VerifiedAdmissionEvidence {
    verifier()
        .verify(&sign(&header(), claims, &root()), &peer(), NOW)
        .unwrap()
}
fn predicate(predicate: PolicyPredicate) -> PolicyExpressionV1 {
    PolicyExpressionV1::Predicate(predicate)
}
fn offline_policy() -> PolicyExpressionV1 {
    predicate(PolicyPredicate::TolerateStalePolicy)
}

#[test]
fn reusable_jwt_preserves_signed_provenance_and_typed_claims() {
    let token = sign(&header(), &claims(), &root());
    for now in [NOW, NOW + 86_400, NOW + 864_000] {
        let evidence = verifier().verify(&token, &peer(), now).unwrap();
        assert_eq!(evidence.original_jwt(), Some(token.as_str()));
        assert_eq!(evidence.source(), AdmissionEvidenceSource::PresentedJwt);
        assert_eq!(evidence.subject(), &peer());
        assert_eq!(evidence.expires_at(), None);
        assert_eq!(
            evidence.claims().attributes.as_ref().unwrap()["workers"],
            TypedAttribute::Int(8)
        );
    }
}

#[test]
fn rejects_copied_credentials_subject_confirmation_and_issuer_substitution() {
    let token = sign(&header(), &claims(), &root());
    let other = SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes();
    assert_eq!(
        verifier().verify(&token, &other, NOW).unwrap_err(),
        EnrollmentError::PeerKeyMismatch
    );
    for field in ["sub", "cnf", "iss"] {
        let mut changed = claims();
        match field {
            "sub" | "iss" => changed[field] = json!(hex::encode(other)),
            _ => changed["cnf"]["jwk"]["x"] = json!(URL_SAFE_NO_PAD.encode(other)),
        }
        assert!(
            verifier()
                .verify(&sign(&header(), &changed, &root()), &peer(), NOW)
                .is_err(),
            "{field}"
        );
    }
    assert!(verifier()
        .verify(
            &sign(&header(), &claims(), &SigningKey::from_bytes(&[9; 32])),
            &peer(),
            NOW
        )
        .is_err());
}

#[test]
fn rejects_algorithm_type_key_selection_and_jose_extensions() {
    for (name, value) in [
        ("alg", json!("none")),
        ("alg", json!("HS256")),
        ("alg", json!("ES256")),
        ("typ", json!("JWT")),
        ("typ", json!("aster-attestation+jwt")),
        ("crit", json!(["custom"])),
        ("b64", json!(false)),
        ("jku", json!("https://invalid.example/keys")),
        ("jwk", json!({})),
        ("kid", json!("attacker")),
        ("x5u", json!("https://invalid.example/cert")),
    ] {
        let mut h = header();
        h[name] = value;
        assert_eq!(
            verifier()
                .verify(&sign(&h, &claims(), &root()), &peer(), NOW)
                .unwrap_err(),
            EnrollmentError::UnsupportedHeader,
            "{name}"
        );
    }
    assert!(verifier()
        .verify(
            &sign_raw(
                br#"{"alg":"EdDSA","typ":"aster-enrollment-v1+jwt","alg":"EdDSA"}"#,
                &serde_json::to_vec(&claims()).unwrap(),
                &root()
            ),
            &peer(),
            NOW
        )
        .is_err());
}

#[test]
fn exact_audience_and_validity_boundaries_optional_expiry() {
    let mut c = claims();
    c["aud"] = json!(["other", "build-fleet"]);
    c["nbf"] = json!(NOW);
    c["exp"] = json!(NOW + 1);
    let token = sign(&header(), &c, &root());
    assert!(verifier().verify(&token, &peer(), NOW).is_ok());
    assert_eq!(
        verifier().verify(&token, &peer(), NOW - 1).unwrap_err(),
        EnrollmentError::InvalidValidity
    );
    assert_eq!(
        verifier().verify(&token, &peer(), NOW + 1).unwrap_err(),
        EnrollmentError::InvalidValidity
    );
    c["aud"] = json!("Build-Fleet");
    assert!(verifier()
        .verify(&sign(&header(), &c, &root()), &peer(), NOW)
        .is_err());
    c["aud"] = json!("build-fleet");
    c["iat"] = json!(NOW + 1);
    assert!(verifier()
        .verify(&sign(&header(), &c, &root()), &peer(), NOW)
        .is_err());
}

#[test]
fn rejects_malformed_missing_restrictions_and_numeric_bounds() {
    for name in [
        "iss",
        "sub",
        "aud",
        "iat",
        "jti",
        "cnf",
        "aster_enrollment_v1",
    ] {
        let mut c = claims();
        c.as_object_mut().unwrap().remove(name);
        assert!(
            verifier()
                .verify(&sign(&header(), &c, &root()), &peer(), NOW)
                .is_err(),
            "{name}"
        );
    }
    let mut c = claims();
    c["aster_enrollment_v1"]["mandatory"] = json!(["https://example.org/restrict-region"]);
    assert_eq!(
        verifier()
            .verify(&sign(&header(), &c, &root()), &peer(), NOW)
            .unwrap_err(),
        EnrollmentError::UnsupportedMandatoryRestriction
    );
    c["aster_enrollment_v1"]
        .as_object_mut()
        .unwrap()
        .remove("mandatory");
    assert!(verifier()
        .verify(&sign(&header(), &c, &root()), &peer(), NOW)
        .is_err());
    for field in ["iat", "nbf", "exp"] {
        for value in [
            Value::Null,
            json!("1800000000"),
            json!(-1),
            json!(NOW as f64 + 0.5),
        ] {
            let mut c = claims();
            c[field] = value;
            assert!(
                verifier()
                    .verify(&sign(&header(), &c, &root()), &peer(), NOW)
                    .is_err(),
                "{field}"
            );
        }
    }
    for value in [
        json!(1.5),
        json!("8"),
        json!(true),
        json!(18446744073709551615u64),
    ] {
        let mut c = claims();
        c["aster_enrollment_v1"]["attributes"]["workers"]["value"] = value;
        assert!(verifier()
            .verify(&sign(&header(), &c, &root()), &peer(), NOW)
            .is_err());
    }
    let mut c = claims();
    c["cnf"]["jwk"]["d"] = json!("private-key-not-permitted");
    assert!(verifier()
        .verify(&sign(&header(), &c, &root()), &peer(), NOW)
        .is_err());
}

#[test]
fn ignores_optional_claims_but_rejects_oversized_tampered_and_legacy_inputs() {
    let mut c = claims();
    c["https://example.org/optional"] = json!({"custom": true});
    assert!(verifier()
        .verify(&sign(&header(), &c, &root()), &peer(), NOW)
        .is_ok());
    let token = sign(&header(), &c, &root());
    let parts: Vec<_> = token.split('.').collect();
    c["jti"] = json!("tampered");
    let tampered = format!(
        "{}.{}.{}",
        parts[0],
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap()),
        parts[2]
    );
    assert!(verifier().verify(&tampered, &peer(), NOW).is_err());
    assert!(verifier()
        .verify("aster.attestation.chain.v1:AAAA", &peer(), NOW)
        .is_err());
    assert_eq!(
        verifier()
            .verify(&"a".repeat(MAX_JWT_BYTES + 1), &peer(), NOW)
            .unwrap_err(),
        EnrollmentError::TokenTooLarge
    );
}

#[test]
fn recursive_composition_and_negation_preserve_unknown_evidence() {
    let policy = PolicyExpressionV1::AllOf(vec![
        predicate(PolicyPredicate::Audience("build-fleet".into())),
        PolicyExpressionV1::AnyOf(vec![
            predicate(PolicyPredicate::Scope("build-worker".into())),
            predicate(PolicyPredicate::Scope("fleet-admin".into())),
        ]),
        PolicyExpressionV1::NoneOf(vec![
            predicate(PolicyPredicate::Tag("quarantined".into())),
            predicate(PolicyPredicate::Tag("retired".into())),
        ]),
    ]);
    let c = claims();
    assert_eq!(
        policy.evaluate(&evidence(&c), &AdmissionContext::default(), NOW),
        Ok(MatchState::Match)
    );
    let mut c = c;
    c["aster_enrollment_v1"]
        .as_object_mut()
        .unwrap()
        .remove("tags");
    assert_eq!(
        policy.evaluate(&evidence(&c), &AdmissionContext::default(), NOW),
        Ok(MatchState::Unknown)
    );
    let nested = PolicyExpressionV1::NoneOf(vec![PolicyExpressionV1::AllOf(vec![
        predicate(PolicyPredicate::Scope("missing".into())),
        predicate(PolicyPredicate::Tag("retired".into())),
    ])]);
    assert_eq!(
        nested.evaluate(&evidence(&c), &AdmissionContext::default(), NOW),
        Ok(MatchState::Unknown)
    );
    c["aster_enrollment_v1"]["tags"] = json!(["quarantined"]);
    assert_eq!(
        policy.evaluate(&evidence(&c), &AdmissionContext::default(), NOW),
        Ok(MatchState::NoMatch)
    );
}

#[test]
fn expression_bounds_and_errors_cannot_be_hidden_by_any_or_none() {
    let e = evidence(&claims());
    for empty in [
        PolicyExpressionV1::AllOf(vec![]),
        PolicyExpressionV1::AnyOf(vec![]),
        PolicyExpressionV1::NoneOf(vec![]),
    ] {
        let expr = PolicyExpressionV1::AnyOf(vec![offline_policy(), empty]);
        assert_eq!(
            expr.evaluate(&e, &AdmissionContext::default(), NOW),
            Err(PolicyError::EmptyCombinator)
        );
    }
    let wide = PolicyExpressionV1::AnyOf(vec![offline_policy(); MAX_POLICY_NODES]);
    assert_eq!(wide.validate(), Err(PolicyError::NodeLimit));
    let mut deep = offline_policy();
    for _ in 1..MAX_POLICY_DEPTH {
        deep = PolicyExpressionV1::NoneOf(vec![deep]);
    }
    assert!(deep.validate().is_ok());
    deep = PolicyExpressionV1::NoneOf(vec![deep]);
    assert_eq!(deep.validate(), Err(PolicyError::DepthLimit));
    let bad = PolicyExpressionV1::NoneOf(vec![predicate(PolicyPredicate::AttributeEquals {
        key: "load".into(),
        value: TypedAttribute::Float(f64::NAN),
    })]);
    assert_eq!(bad.validate(), Err(PolicyError::InvalidPredicate));
}

#[test]
fn attributes_remain_typed_and_absence_differs_from_unknown() {
    let c = claims();
    let e = evidence(&c);
    for (expected, matched) in [
        (TypedAttribute::Int(8), true),
        (TypedAttribute::Float(8.0), false),
        (TypedAttribute::String("8".into()), false),
        (TypedAttribute::Bool(true), false),
    ] {
        let expr = predicate(PolicyPredicate::AttributeEquals {
            key: "workers".into(),
            value: expected,
        });
        assert_eq!(
            expr.evaluate(&e, &AdmissionContext::default(), NOW)
                .unwrap(),
            if matched {
                MatchState::Match
            } else {
                MatchState::NoMatch
            }
        );
    }
    let expr = PolicyExpressionV1::NoneOf(vec![predicate(PolicyPredicate::AttributeEquals {
        key: "absent".into(),
        value: TypedAttribute::Bool(true),
    })]);
    assert_eq!(
        expr.evaluate(&e, &AdmissionContext::default(), NOW),
        Ok(MatchState::Match)
    );
    let mut c = c;
    c["aster_enrollment_v1"]
        .as_object_mut()
        .unwrap()
        .remove("attributes");
    assert_eq!(
        expr.evaluate(&evidence(&c), &AdmissionContext::default(), NOW),
        Ok(MatchState::Unknown)
    );
}

#[tokio::test]
async fn consumer_selects_offline_or_fresh_policy_without_invented_freshness() {
    let e = evidence(&claims());
    let revocations = KnownRevocations::default();
    let mut context = AdmissionContext::default();
    let fresh = predicate(PolicyPredicate::PolicySynchronizedWithin {
        max_age: Duration::from_secs(60),
        require_root: false,
    });
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &offline_policy(),
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Allow
    );
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &fresh,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::MissingEvidence)
    );
    context.policy_sync = Some(PolicySynchronization {
        completed_at: NOW - 60,
        source: SynchronizationSource::Replica([1; 32]),
        observed_revision: Some(42),
    });
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &fresh,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Allow
    );
    let root_only = predicate(PolicyPredicate::PolicySynchronizedWithin {
        max_age: Duration::from_secs(60),
        require_root: true,
    });
    assert_eq!(
        root_only.evaluate(&e, &context, NOW),
        Ok(MatchState::NoMatch)
    );
    assert_eq!(
        fresh.evaluate(&e, &context, NOW + 1),
        Ok(MatchState::NoMatch)
    );
    context.pending_policy_content = true;
    assert_eq!(fresh.evaluate(&e, &context, NOW), Ok(MatchState::Unknown));
    context.pending_policy_content = false;
    context.policy_sync.as_mut().unwrap().completed_at = NOW + 1;
    assert_eq!(fresh.evaluate(&e, &context, NOW), Ok(MatchState::Unknown));
}

struct Permissive;
#[async_trait]
impl AdmissionPolicy for Permissive {
    async fn evaluate(
        &self,
        _: &VerifiedAdmissionEvidence,
        _: &AdmissionContext,
        _: u64,
    ) -> Result<AdmissionDecision, PolicyError> {
        Ok(AdmissionDecision::Allow)
    }
}
struct Failing;
#[async_trait]
impl AdmissionPolicy for Failing {
    async fn evaluate(
        &self,
        _: &VerifiedAdmissionEvidence,
        _: &AdmissionContext,
        _: u64,
    ) -> Result<AdmissionDecision, PolicyError> {
        Err(PolicyError::Handler("backend unavailable".into()))
    }
}
struct Pending;
#[async_trait]
impl AdmissionPolicy for Pending {
    async fn evaluate(
        &self,
        _: &VerifiedAdmissionEvidence,
        _: &AdmissionContext,
        _: u64,
    ) -> Result<AdmissionDecision, PolicyError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn revocations_are_unconditional_and_issuer_scoped() {
    let e = evidence(&claims());
    let context = AdmissionContext::default();
    let revocations = KnownRevocations::default();
    revocations.revoke_node([1; 32], peer());
    revocations.revoke_credential([1; 32], e.credential_id().unwrap());
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &Permissive,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Allow
    );
    revocations.revoke_credential(*e.issuer(), e.credential_id().unwrap());
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &Permissive,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::CredentialRevoked)
    );
    let mut replacement = claims();
    replacement["jti"] = json!("replacement");
    let replacement = evidence(&replacement);
    assert_eq!(
        admit(
            &replacement,
            &revocations,
            &context,
            &Permissive,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Allow
    );
    revocations.revoke_node(*e.issuer(), peer());
    for evidence in [&e, &replacement] {
        assert_eq!(
            admit(
                evidence,
                &revocations,
                &context,
                &Permissive,
                Duration::from_secs(1),
                || NOW
            )
            .await,
            AdmissionDecision::Deny(DenialReason::NodeRevoked)
        );
    }
}

#[tokio::test]
async fn handler_errors_timeouts_and_delayed_expiry_deny() {
    let mut c = claims();
    c["exp"] = json!(NOW + 1);
    let e = evidence(&c);
    let revocations = KnownRevocations::default();
    let context = AdmissionContext::default();
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &Failing,
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::PolicyError(PolicyError::Handler(
            "backend unavailable".into()
        )))
    );
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &Pending,
            Duration::from_millis(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::PolicyTimeout)
    );
    let clock = AtomicU64::new(NOW);
    assert_eq!(
        admit(
            &e,
            &revocations,
            &context,
            &Permissive,
            Duration::from_secs(1),
            || clock.fetch_add(1, Ordering::SeqCst)
        )
        .await,
        AdmissionDecision::Deny(DenialReason::InvalidValidity)
    );
}

#[tokio::test]
async fn revocation_learned_during_handler_wins_over_allow() {
    struct Revoke<'a>(&'a KnownRevocations);
    #[async_trait]
    impl AdmissionPolicy for Revoke<'_> {
        async fn evaluate(
            &self,
            evidence: &VerifiedAdmissionEvidence,
            _: &AdmissionContext,
            _: u64,
        ) -> Result<AdmissionDecision, PolicyError> {
            self.0.revoke_node(*evidence.issuer(), *evidence.subject());
            Ok(AdmissionDecision::Allow)
        }
    }
    let e = evidence(&claims());
    let revocations = KnownRevocations::default();
    assert_eq!(
        admit(
            &e,
            &revocations,
            &AdmissionContext::default(),
            &Revoke(&revocations),
            Duration::from_secs(1),
            || NOW
        )
        .await,
        AdmissionDecision::Deny(DenialReason::NodeRevoked)
    );
}
