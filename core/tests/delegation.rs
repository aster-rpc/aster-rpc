#![cfg(feature = "delegation")]
//! Security invariants INV-A1–A16 and A18–A19 of
//! docs/_internal/ownership-attestations.md §10, one test per invariant.
//! Hostile links are signed directly so the verifier, not issuance, is tested.

use std::collections::BTreeSet;

use aster_transport_core::delegation::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

const AUD: &str = "fleet";
const NOW: u64 = 1_000_000;
const DAY: u64 = 86_400;

/// A mutation that makes a well-formed link payload hostile.
type Edit = Box<dyn Fn(&mut Value)>;

fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}

fn key(n: u8) -> [u8; 32] {
    ed25519_dalek::SigningKey::from_bytes(&seed(n))
        .verifying_key()
        .to_bytes()
}

const ROOT: u8 = 1;
const ISSUER: u8 = 2;
const NODE: u8 = 3;
const OTHER: u8 = 4;

fn set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn grant() -> DelegationGrant {
    DelegationGrant {
        may_issue: [LinkKind::Delegation, LinkKind::Enrollment, LinkKind::Action].into(),
        audiences: set(&[AUD]),
        scopes: set(&["build", "deploy"]),
        tags: set(&["linux"]),
        attribute_keys: set(&["role"]),
        max_child_lifetime: 7 * DAY,
        max_depth: 1,
        subjects: None,
        operations: set(&["exec.submit"]),
        mandatory: BTreeSet::new(),
        may_register: false,
    }
}

fn enrollment(scopes: &[&str]) -> EnrollmentExtensionV1 {
    EnrollmentExtensionV1 {
        claims: AdmissionClaims {
            scopes: Some(set(scopes)),
            tags: None,
            attributes: None,
        },
        mandatory: vec![],
        registration: Registration::Ephemeral,
    }
}

fn aud() -> Audience {
    Audience::One(AUD.into())
}

fn validity(issued_at: u64, lifetime: u64) -> Validity {
    Validity {
        issued_at,
        not_before: None,
        expires_at: issued_at + lifetime,
    }
}

/// root → issuer (delegation), issued through the public API.
fn delegation_to_issuer(grant: &DelegationGrant) -> Chain {
    issue_delegation(
        &seed(ROOT),
        None,
        &key(ISSUER),
        &aud(),
        grant,
        validity(NOW - 100, 20 * DAY),
    )
    .unwrap()
}

/// root → issuer → node (enrollment).
fn node_chain() -> Chain {
    let parent = delegation_to_issuer(&grant());
    issue_enrollment(
        &seed(ISSUER),
        Some(&parent),
        &key(NODE),
        &aud(),
        &enrollment(&["build"]),
        validity(NOW - 10, DAY),
    )
    .unwrap()
}

fn anchors() -> [[u8; 32]; 1] {
    [key(ROOT)]
}

fn verify(chain: &str, leaf: u8) -> Result<VerifiedChain, ChainError> {
    let anchors = anchors();
    verify_with(
        chain,
        leaf,
        &TrustContext::new(&anchors, AUD, NOW, &NoRevocations),
    )
}

fn verify_with(
    chain: &str,
    leaf: u8,
    trust: &TrustContext<'_>,
) -> Result<VerifiedChain, ChainError> {
    verify_chain(chain.as_bytes(), LeafKind::Enrollment, &key(leaf), trust)
}

/// Sign arbitrary header and payload JSON, bypassing issuance checks.
fn raw_jws(signer: u8, header: &Value, payload: &Value) -> String {
    use ed25519_dalek::Signer;
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap())
    );
    let signature = ed25519_dalek::SigningKey::from_bytes(&seed(signer)).sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

fn cnf(n: u8) -> Value {
    json!({"jwk": {"kty": "OKP", "crv": "Ed25519", "x": URL_SAFE_NO_PAD.encode(key(n))}})
}

/// A well-formed link payload; tests mutate fields to make it hostile.
fn payload(issuer: u8, subject: u8, kind: LinkKind, iat: u64, exp: u64) -> Value {
    let mut value = json!({
        "iss": hex::encode(key(issuer)),
        "sub": hex::encode(key(subject)),
        "cnf": cnf(subject),
        "aud": AUD,
        "jti": format!("jti-{issuer}-{subject}-{iat}-xxxxxxxx"),
        "iat": iat,
        "exp": exp,
    });
    let extension = match kind {
        LinkKind::Delegation => (
            "aster_delegation_v1",
            serde_json::to_value(grant()).unwrap(),
        ),
        LinkKind::Enrollment => (
            "aster_enrollment_v1",
            serde_json::to_value(enrollment(&["build"])).unwrap(),
        ),
        LinkKind::Action => (
            "aster_action_v1",
            json!({"operations": ["exec.submit"], "resources": [], "mandatory": []}),
        ),
    };
    value[extension.0] = extension.1;
    value
}

fn header(kind: LinkKind) -> Value {
    json!({"alg": "EdDSA", "typ": kind.jwt_type()})
}

fn link(signer: u8, payload: &Value) -> String {
    let typ = if payload.get("aster_delegation_v1").is_some() {
        LinkKind::Delegation
    } else if payload.get("aster_action_v1").is_some() {
        LinkKind::Action
    } else {
        LinkKind::Enrollment
    };
    raw_jws(signer, &header(typ), payload)
}

fn chain(links: &[String]) -> String {
    format!("{CHAIN_PREFIX}~{}", links.join("~"))
}

/// root → issuer delegation link, signed raw.
fn raw_delegation() -> String {
    raw_delegation_until(NOW + 20 * DAY)
}

fn raw_delegation_until(exp: u64) -> String {
    link(
        ROOT,
        &payload(ROOT, ISSUER, LinkKind::Delegation, NOW - 100, exp),
    )
}

fn raw_leaf(edit: impl FnOnce(&mut Value)) -> String {
    let mut value = payload(ISSUER, NODE, LinkKind::Enrollment, NOW - 10, NOW + DAY);
    edit(&mut value);
    link(ISSUER, &value)
}

#[test]
fn issued_chains_verify_and_expose_their_evidence() {
    let chain = node_chain();
    let verified = verify(chain.to_text(), NODE).unwrap();
    assert_eq!(verified.anchor(), &key(ROOT));
    assert_eq!(verified.issuers(), &[key(ISSUER), key(ROOT)]);
    assert_eq!(verified.subject(), &key(NODE));
    assert_eq!(verified.not_after(), NOW - 10 + DAY);
    assert_eq!(verified.jtis().len(), 2);
    assert_eq!(verified.digest(), &chain.digest());
    assert_eq!(
        verified.leaf(),
        &LeafClaims::Enrollment(enrollment(&["build"]))
    );
    // A one-link chain from the root is today's enrollment credential shape.
    let direct = issue_enrollment(
        &seed(ROOT),
        None,
        &key(NODE),
        &aud(),
        &enrollment(&[]),
        validity(NOW, DAY),
    )
    .unwrap();
    assert_eq!(
        verify(direct.to_text(), NODE).unwrap().issuers(),
        &[key(ROOT)]
    );
}

#[test]
fn inv_a1_oversize_input_is_rejected_before_any_parsing_or_signature_work() {
    let valid = node_chain();
    let links: Vec<String> = valid.links().map(str::to_owned).collect();
    #[cfg(feature = "test-hooks")]
    let before = test_counters::snapshot();
    let huge = format!("{CHAIN_PREFIX}~{}", "a".repeat(MAX_CHAIN_BYTES));
    assert_eq!(verify(&huge, NODE).unwrap_err(), ChainError::Size);
    let long_link = chain(&[format!("{}.{}", links[0], "a".repeat(MAX_LINK_BYTES))]);
    assert_eq!(verify(&long_link, NODE).unwrap_err(), ChainError::Size);
    let too_many = chain(&vec![links[0].clone(); DEFAULT_MAX_LINKS + 1]);
    assert_eq!(verify(&too_many, NODE).unwrap_err(), ChainError::Size);
    #[cfg(feature = "test-hooks")]
    assert_eq!(
        test_counters::snapshot(),
        before,
        "no link decoded or verified"
    );
}

#[test]
fn inv_a2_headers_never_select_keys_or_algorithms() {
    let delegation = raw_delegation();
    let body = payload(ISSUER, NODE, LinkKind::Enrollment, NOW - 10, NOW + DAY);
    for (bad, expected) in [
        (
            json!({"alg": "EdDSA", "typ": ENROLLMENT_LINK_TYPE, "kid": "x"}),
            ChainError::Header,
        ),
        (
            json!({"alg": "EdDSA", "typ": ENROLLMENT_LINK_TYPE, "jwk": cnf(OTHER)["jwk"]}),
            ChainError::Header,
        ),
        (
            json!({"alg": "ES256", "typ": ENROLLMENT_LINK_TYPE}),
            ChainError::Header,
        ),
        (
            json!({"alg": "none", "typ": ENROLLMENT_LINK_TYPE}),
            ChainError::Header,
        ),
        (json!({"alg": "EdDSA", "typ": "JWT"}), ChainError::Header),
    ] {
        let leaf = raw_jws(ISSUER, &bad, &body);
        assert_eq!(
            verify(&chain(&[leaf, delegation.clone()]), NODE).unwrap_err(),
            expected,
            "{bad}"
        );
    }
}

#[test]
fn inv_a3_spliced_chain_fails_bridge_before_any_signature_check() {
    // Two genuine chains: root → issuer → node and root → other → node.
    let genuine = node_chain();
    let other_parent = issue_delegation(
        &seed(ROOT),
        None,
        &key(OTHER),
        &aud(),
        &grant(),
        validity(NOW - 100, 20 * DAY),
    )
    .unwrap();
    // Splice the issuer-signed leaf onto the other delegation. Every link
    // carries a real signature.
    let leaf = genuine.links().next().unwrap().to_owned();
    let spliced = chain(&[leaf, other_parent.links().next().unwrap().to_owned()]);
    #[cfg(feature = "test-hooks")]
    let (_, verified_before) = test_counters::snapshot();
    assert_eq!(verify(&spliced, NODE).unwrap_err(), ChainError::Bridge);
    #[cfg(feature = "test-hooks")]
    assert_eq!(
        test_counters::snapshot().1,
        verified_before,
        "no signature verified"
    );
}

#[test]
fn inv_a4_leaf_must_name_and_confirm_the_authenticated_key() {
    let chain_text = node_chain().into_text();
    assert_eq!(
        verify(&chain_text, OTHER).unwrap_err(),
        ChainError::LeafMismatch
    );
    let leaf = raw_leaf(|value| value["cnf"] = cnf(OTHER));
    assert_eq!(
        verify(&chain(&[leaf, raw_delegation()]), NODE).unwrap_err(),
        ChainError::LeafMismatch
    );
}

#[test]
fn inv_a5_untrusted_anchor_is_rejected() {
    let chain_text = node_chain().into_text();
    let anchors = [key(OTHER)];
    let trust = TrustContext::new(&anchors, AUD, NOW, &NoRevocations);
    assert_eq!(
        verify_with(&chain_text, NODE, &trust).unwrap_err(),
        ChainError::UntrustedAnchor
    );
}

#[test]
fn inv_a6_leaf_beyond_any_ancestor_bound_is_rejected() {
    let parent = delegation_to_issuer(&grant());
    let cases: Vec<Edit> = vec![
        Box::new(|v| v["aster_enrollment_v1"]["scopes"] = json!(["admin"])),
        Box::new(|v| v["aster_enrollment_v1"]["tags"] = json!(["windows"])),
        Box::new(|v| {
            v["aster_enrollment_v1"]["attributes"] =
                json!({"zone": {"type": "string", "value": "eu"}})
        }),
        Box::new(|v| v["aud"] = json!([AUD, "other-fleet"])),
    ];
    for edit in cases {
        let leaf = raw_leaf(|value| edit(value));
        let text = chain(&[leaf, parent.links().next().unwrap().to_owned()]);
        assert_eq!(verify(&text, NODE).unwrap_err(), ChainError::Attenuation);
    }
    // An action leaf claiming an operation the delegation does not allow.
    let mut action = payload(ISSUER, NODE, LinkKind::Action, NOW - 10, NOW + DAY);
    action["aster_action_v1"]["operations"] = json!(["exec.kill"]);
    let text = chain(&[link(ISSUER, &action), raw_delegation()]);
    let anchors = anchors();
    let trust = TrustContext::new(&anchors, AUD, NOW, &NoRevocations);
    assert_eq!(
        verify_chain(text.as_bytes(), LeafKind::Action, &key(NODE), &trust).unwrap_err(),
        ChainError::Attenuation
    );
    // Issuance refuses the same thing.
    assert_eq!(
        issue_enrollment(
            &seed(ISSUER),
            Some(&parent),
            &key(NODE),
            &aud(),
            &enrollment(&["admin"]),
            validity(NOW, DAY)
        )
        .unwrap_err(),
        ChainError::Attenuation
    );
}

#[test]
fn inv_a7_child_lifetime_and_issue_time_stay_within_the_parent() {
    let cases: Vec<(u64, Edit)> = vec![
        // Longer than max_child_lifetime (7 days).
        (
            NOW + 20 * DAY,
            Box::new(|v| v["exp"] = json!(NOW - 10 + 8 * DAY)),
        ),
        // Within max_child_lifetime but outlives its parent.
        (NOW + DAY, Box::new(|v| v["exp"] = json!(NOW + 2 * DAY))),
        // Issued before its parent was.
        (NOW + 20 * DAY, Box::new(|v| v["iat"] = json!(NOW - 200))),
        // Issued after its parent expired (within leeway of now).
        (
            NOW + 30,
            Box::new(|v| {
                v["iat"] = json!(NOW + 40);
                v["exp"] = json!(NOW + 50)
            }),
        ),
    ];
    for (parent_exp, edit) in cases {
        let leaf = raw_leaf(|value| edit(value));
        assert_eq!(
            verify(&chain(&[leaf, raw_delegation_until(parent_exp)]), NODE).unwrap_err(),
            ChainError::Attenuation
        );
    }
}

#[test]
fn inv_a8_depth_and_may_issue_bound_the_subtree() {
    // Sub-delegation below a max_depth 0 parent.
    let mut leaves_only = grant();
    leaves_only.max_depth = 0;
    let parent = delegation_to_issuer(&leaves_only);
    let mut sub = payload(ISSUER, OTHER, LinkKind::Delegation, NOW - 10, NOW + DAY);
    sub["aster_delegation_v1"]["max_depth"] = json!(0);
    let leaf = {
        let value = payload(OTHER, NODE, LinkKind::Enrollment, NOW - 5, NOW + DAY / 2);
        link(OTHER, &value)
    };
    let text = chain(&[
        leaf.clone(),
        link(ISSUER, &sub),
        parent.links().next().unwrap().to_owned(),
    ]);
    assert_eq!(verify(&text, NODE).unwrap_err(), ChainError::Attenuation);
    // A leaf kind the parent may not issue.
    let mut actions_only = grant();
    actions_only.may_issue = [LinkKind::Action].into();
    let parent = delegation_to_issuer(&actions_only);
    let text = chain(&[raw_leaf(|_| {}), parent.links().next().unwrap().to_owned()]);
    assert_eq!(verify(&text, NODE).unwrap_err(), ChainError::Attenuation);
    // A sub-delegate granting itself a kind its parent lacks.
    let mut delegate_actions = grant();
    delegate_actions.may_issue = [LinkKind::Delegation, LinkKind::Action].into();
    let parent = delegation_to_issuer(&delegate_actions);
    let mut sub = payload(ISSUER, OTHER, LinkKind::Delegation, NOW - 10, NOW + DAY);
    sub["aster_delegation_v1"]["max_depth"] = json!(0);
    sub["aster_delegation_v1"]["may_issue"] = json!(["action", "enrollment"]);
    let mut action = payload(OTHER, NODE, LinkKind::Action, NOW - 5, NOW + DAY / 2);
    action["aster_action_v1"]["operations"] = json!(["exec.submit"]);
    let text = chain(&[
        link(OTHER, &action),
        link(ISSUER, &sub),
        parent.links().next().unwrap().to_owned(),
    ]);
    let anchors = anchors();
    let trust = TrustContext::new(&anchors, AUD, NOW, &NoRevocations);
    assert_eq!(
        verify_chain(text.as_bytes(), LeafKind::Action, &key(NODE), &trust).unwrap_err(),
        ChainError::Attenuation
    );
}

#[test]
fn inv_e26_persistent_registration_needs_may_register_down_the_chain() {
    let persistent = EnrollmentExtensionV1 {
        registration: Registration::Persistent,
        ..enrollment(&["build"])
    };
    // An issuer without may_register cannot sign a persistent leaf...
    let parent = delegation_to_issuer(&grant());
    assert_eq!(
        issue_enrollment(
            &seed(ISSUER),
            Some(&parent),
            &key(NODE),
            &aud(),
            &persistent,
            validity(NOW - 10, DAY),
        )
        .unwrap_err(),
        ChainError::Attenuation
    );
    // ...and a verifier rejects one signed around the issuance check.
    let leaf = raw_leaf(|value| {
        value["aster_enrollment_v1"]["registration"] = json!("persistent");
    });
    let text = chain(&[leaf, parent.links().next().unwrap().to_owned()]);
    assert_eq!(verify(&text, NODE).unwrap_err(), ChainError::Attenuation);

    // With may_register the same leaf is issued, verifies and says so.
    let mut registering = grant();
    registering.may_register = true;
    let parent = delegation_to_issuer(&registering);
    let issued = issue_enrollment(
        &seed(ISSUER),
        Some(&parent),
        &key(NODE),
        &aud(),
        &persistent,
        validity(NOW - 10, DAY),
    )
    .unwrap();
    let verified = verify(issued.to_text(), NODE).unwrap();
    assert!(matches!(
        verified.leaf(),
        LeafClaims::Enrollment(extension) if extension.registration == Registration::Persistent
    ));
    // An ephemeral leaf needs no permission, and omission means ephemeral.
    let leaf = raw_leaf(|value| {
        value["aster_enrollment_v1"]
            .as_object_mut()
            .unwrap()
            .remove("registration");
    });
    let text = chain(&[
        leaf,
        delegation_to_issuer(&grant())
            .links()
            .next()
            .unwrap()
            .to_owned(),
    ]);
    assert!(matches!(
        verify(&text, NODE).unwrap().leaf(),
        LeafClaims::Enrollment(extension) if extension.registration == Registration::Ephemeral
    ));

    // A sub-delegate cannot grant may_register its parent lacks.
    let parent = delegation_to_issuer(&grant());
    let mut sub = payload(ISSUER, OTHER, LinkKind::Delegation, NOW - 10, NOW + DAY);
    sub["aster_delegation_v1"]["max_depth"] = json!(0);
    sub["aster_delegation_v1"]["may_register"] = json!(true);
    let leaf = link(
        OTHER,
        &payload(OTHER, NODE, LinkKind::Enrollment, NOW - 5, NOW + DAY / 2),
    );
    let text = chain(&[
        leaf,
        link(ISSUER, &sub),
        parent.links().next().unwrap().to_owned(),
    ]);
    assert_eq!(verify(&text, NODE).unwrap_err(), ChainError::Attenuation);
    // A root-issued (one-link) persistent credential needs no delegation.
    let root_issued = issue_enrollment(
        &seed(ROOT),
        None,
        &key(NODE),
        &aud(),
        &persistent,
        validity(NOW - 10, DAY),
    )
    .unwrap();
    verify(root_issued.to_text(), NODE).unwrap();
}

#[test]
fn inv_a9_ancestor_restrictions_accumulate_and_unknown_ones_reject() {
    let mut restricted = grant();
    restricted.mandatory = set(&["source_cidr"]);
    let parent = delegation_to_issuer(&restricted);
    let child = issue_enrollment(
        &seed(ISSUER),
        Some(&parent),
        &key(NODE),
        &aud(),
        &enrollment(&["build"]),
        validity(NOW, DAY),
    )
    .unwrap();
    // The leaf names no restriction, yet it is bound by its ancestor's.
    assert_eq!(
        verify(child.to_text(), NODE).unwrap_err(),
        ChainError::UnsupportedRestriction
    );
    let anchors = anchors();
    let mut trust = TrustContext::new(&anchors, AUD, NOW, &NoRevocations);
    trust.supported_restrictions = &["source_cidr"];
    let verified = verify_with(child.to_text(), NODE, &trust).unwrap();
    assert_eq!(verified.mandatory(), &set(&["source_cidr"]));
}

#[test]
fn inv_a10_every_link_expires_and_expiry_has_no_leeway() {
    let missing = raw_leaf(|value| {
        value.as_object_mut().unwrap().remove("exp");
    });
    assert_eq!(
        verify(&chain(&[missing, raw_delegation()]), NODE).unwrap_err(),
        ChainError::Malformed
    );
    let null_exp = raw_leaf(|value| value["exp"] = Value::Null);
    assert_eq!(
        verify(&chain(&[null_exp, raw_delegation()]), NODE).unwrap_err(),
        ChainError::Malformed
    );
    // exp == now is expired even though leeway is 60 s.
    let expired = raw_leaf(|value| value["exp"] = json!(NOW));
    assert_eq!(
        verify(&chain(&[expired, raw_delegation()]), NODE).unwrap_err(),
        ChainError::Expired
    );
    let mut long = payload(ROOT, ISSUER, LinkKind::Delegation, NOW - 100, 0);
    long["iss"] = json!(hex::encode(key(ROOT)));
    long["exp"] = json!(NOW - 100 + MAX_LINK_LIFETIME + 1);
    assert_eq!(
        verify(&chain(&[raw_leaf(|_| {}), link(ROOT, &long)]), NODE).unwrap_err(),
        ChainError::Lifetime
    );
}

#[test]
fn inv_a11_activation_tolerates_leeway_and_no_more() {
    for (field, offset, ok) in [
        ("iat", DEFAULT_LEEWAY_SECONDS, true),
        ("iat", DEFAULT_LEEWAY_SECONDS + 1, false),
        ("nbf", DEFAULT_LEEWAY_SECONDS, true),
        ("nbf", DEFAULT_LEEWAY_SECONDS + 1, false),
    ] {
        let leaf = raw_leaf(|value| value[field] = json!(NOW + offset));
        let result = verify(&chain(&[leaf, raw_delegation()]), NODE);
        if ok {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err(), ChainError::NotYetValid, "{field}");
        }
    }
}

struct Revoked([u8; 32]);

impl RevokedDelegates for Revoked {
    fn is_revoked(&self, key: &[u8; 32]) -> bool {
        *key == self.0
    }
}

#[test]
fn inv_a12_revoked_delegate_rejects_before_signatures() {
    let chain_text = node_chain().into_text();
    let anchors = anchors();
    let revoked = Revoked(key(ISSUER));
    let trust = TrustContext::new(&anchors, AUD, NOW, &revoked);
    #[cfg(feature = "test-hooks")]
    let (_, verified_before) = test_counters::snapshot();
    assert_eq!(
        verify_with(&chain_text, NODE, &trust).unwrap_err(),
        ChainError::Revoked
    );
    #[cfg(feature = "test-hooks")]
    assert_eq!(test_counters::snapshot().1, verified_before);
}

#[test]
fn inv_a13_misordered_links_are_rejected_not_reordered() {
    let links: Vec<String> = node_chain().links().map(str::to_owned).collect();
    let reversed = chain(&[links[1].clone(), links[0].clone()]);
    assert_eq!(verify(&reversed, NODE).unwrap_err(), ChainError::Grammar);
}

#[test]
fn inv_a14_noncanonical_encodings_are_rejected() {
    let links: Vec<String> = node_chain().links().map(str::to_owned).collect();
    let delegation = links[1].clone();
    let segments: Vec<&str> = links[0].split('.').collect();
    let duplicate = {
        let body = br#"{"iss":"a","iss":"b"}"#;
        format!(
            "{}.{}.{}",
            segments[0],
            URL_SAFE_NO_PAD.encode(body),
            segments[2]
        )
    };
    let padded = format!("{}=.{}.{}", segments[0], segments[1], segments[2]);
    let spaced = format!("{}. {}.{}", segments[0], segments[1], segments[2]);
    let short_signature = format!(
        "{}.{}.{}",
        segments[0],
        segments[1],
        URL_SAFE_NO_PAD.encode([0u8; 63])
    );
    // The signature's final character carries 4 unused bits; setting one
    // gives an alternative spelling of the same 64 bytes.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let signature = segments[2];
    let last = signature.as_bytes()[signature.len() - 1];
    let index = ALPHABET.iter().position(|c| *c == last).unwrap();
    let alias = format!(
        "{}.{}.{}{}",
        segments[0],
        segments[1],
        &signature[..signature.len() - 1],
        ALPHABET[index | 1] as char
    );
    for bad in [duplicate, padded, spaced, short_signature, alias] {
        assert_eq!(
            verify(&chain(&[bad.clone(), delegation.clone()]), NODE).unwrap_err(),
            ChainError::Malformed,
            "{bad}"
        );
    }
    for bad in [
        format!("{CHAIN_PREFIX}~{}~", links[0]),
        format!("{CHAIN_PREFIX}~~{}", links[0]),
        format!("{CHAIN_PREFIX} ~{}", links[0]),
        format!("aster-chain-v2~{}", links[0]),
    ] {
        assert_eq!(
            verify(&bad, NODE).unwrap_err(),
            ChainError::Malformed,
            "{bad}"
        );
    }
}

/// INV-A15: for random leaves, issuance refuses exactly what the verifier
/// rejects for the same subject, audience and time.
mod inv_a15 {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn issuer_refusals_match_verifier_rejections(
            scopes in proptest::sample::subsequence(vec!["build", "deploy", "admin"], 0..=3),
            lifetime in 1u64..10 * DAY,
            iat_offset in 0u64..200,
            extra_audience in any::<bool>(),
            max_depth in 0u32..2,
            delegate in any::<bool>(),
        ) {
            let parent = delegation_to_issuer(&grant());
            let issued_at = NOW - iat_offset;
            let audience = if extra_audience {
                Audience::Many(vec![AUD.into(), "other".into()])
            } else {
                aud()
            };
            let mut value = payload(ISSUER, NODE, if delegate { LinkKind::Delegation } else { LinkKind::Enrollment }, issued_at, issued_at + lifetime);
            value["aud"] = serde_json::to_value(&audience).unwrap();
            let issued = if delegate {
                let mut sub = grant();
                sub.max_depth = max_depth;
                sub.scopes = set(&scopes);
                value["aster_delegation_v1"] = serde_json::to_value(&sub).unwrap();
                issue_delegation(&seed(ISSUER), Some(&parent), &key(NODE), &audience, &sub, validity(issued_at, lifetime))
            } else {
                value["aster_enrollment_v1"] = serde_json::to_value(enrollment(&scopes)).unwrap();
                issue_enrollment(&seed(ISSUER), Some(&parent), &key(NODE), &audience, &enrollment(&scopes), validity(issued_at, lifetime))
            };
            // The same link signed raw, then checked by the verifier's gates at
            // issue time. A delegation leaf is checked as the parent of a
            // trivially valid enrollment leaf below it.
            let raw = link(ISSUER, &value);
            let parent_link = parent.links().next().unwrap().to_owned();
            let verified = if delegate {
                let leaf = link(NODE, &{
                    let mut leaf = payload(NODE, OTHER, LinkKind::Enrollment, issued_at, issued_at + 1);
                    leaf["aster_enrollment_v1"] = serde_json::to_value(enrollment(&[])).unwrap();
                    leaf
                });
                let anchors = anchors();
                let trust = TrustContext::new(&anchors, AUD, issued_at, &NoRevocations);
                verify_chain(chain(&[leaf, raw, parent_link]).as_bytes(), LeafKind::Enrollment, &key(OTHER), &trust).map(|_| ())
            } else {
                let anchors = anchors();
                let trust = TrustContext::new(&anchors, AUD, issued_at, &NoRevocations);
                verify_chain(chain(&[raw, parent_link]).as_bytes(), LeafKind::Enrollment, &key(NODE), &trust).map(|_| ())
            };
            // Extra audiences are checked per audience by issuance; the
            // verifier for AUD sees them only through attenuation.
            prop_assert_eq!(issued.map(|_| ()), verified);
        }
    }
}

#[test]
fn inv_a16_only_the_committed_next_root_can_rotate() {
    const NEXT: u8 = 10;
    const AFTER: u8 = 11;
    let namespace = [9; 32];
    let current = RootState {
        root: key(ROOT),
        next_commitment: next_root_commitment(&key(NEXT)),
        seq: 0,
    };
    let following = next_root_commitment(&key(AFTER));
    let statement = issue_root_rotation(
        &seed(NEXT),
        &key(ROOT),
        1,
        &following,
        &[namespace],
        NOW,
        RotationReason::Compromise,
    )
    .unwrap();
    let rotation = verify_root_rotation(&statement, &current, NOW, 60).unwrap();
    let next = rotation.apply();
    assert_eq!(next.root, key(NEXT));
    assert_eq!(next.next_commitment, following);
    assert_eq!(rotation.namespaces(), &[namespace]);
    // Signed by the current (possibly compromised) root.
    let by_root = issue_root_rotation(
        &seed(ROOT),
        &key(ROOT),
        1,
        &following,
        &[namespace],
        NOW,
        RotationReason::Compromise,
    )
    .unwrap();
    assert_eq!(
        verify_root_rotation(&by_root, &current, NOW, 60).unwrap_err(),
        ChainError::UntrustedAnchor
    );
    // Wrong predecessor, skipped sequence, replay after applying.
    let wrong_prev = issue_root_rotation(
        &seed(NEXT),
        &key(OTHER),
        1,
        &following,
        &[namespace],
        NOW,
        RotationReason::Routine,
    )
    .unwrap();
    assert_eq!(
        verify_root_rotation(&wrong_prev, &current, NOW, 60).unwrap_err(),
        ChainError::Bridge
    );
    let skipped = issue_root_rotation(
        &seed(NEXT),
        &key(ROOT),
        2,
        &following,
        &[namespace],
        NOW,
        RotationReason::Routine,
    )
    .unwrap();
    assert_eq!(
        verify_root_rotation(&skipped, &current, NOW, 60).unwrap_err(),
        ChainError::Sequence
    );
    assert!(verify_root_rotation(&statement, &next, NOW, 60).is_err());
}

#[test]
fn inv_a18_no_anchor_means_no_verification() {
    let chain_text = node_chain().into_text();
    let trust = TrustContext::new(&[], AUD, NOW, &NoRevocations);
    assert_eq!(
        verify_with(&chain_text, NODE, &trust).unwrap_err(),
        ChainError::Configuration
    );
    let anchors = anchors();
    let mut trust = TrustContext::new(&anchors, AUD, NOW, &NoRevocations);
    trust.leeway = MAX_LEEWAY_SECONDS + 1;
    assert_eq!(
        verify_with(&chain_text, NODE, &trust).unwrap_err(),
        ChainError::Configuration
    );
}

// INV-A19 is the `compile_fail` doc test on `VerifiedChain`: its fields are
// private and `verify_chain` is its only constructor.

#[cfg(feature = "enrollment")]
#[test]
fn a_root_issued_enrollment_credential_is_a_one_link_chain() {
    use aster_transport_core::enrollment::{EnrollmentIssueRequest, JwtEnrollmentIssuer};
    let request = EnrollmentIssueRequest::new(key(NODE), AUD, NOW + DAY);
    let credential = JwtEnrollmentIssuer::from_seed(&seed(ROOT))
        .issue(&request, NOW)
        .unwrap();
    assert_eq!(credential.chain().matches('~').count(), 1);
    let verified = verify(credential.chain(), NODE).unwrap();
    assert_eq!(verified.issuers(), &[key(ROOT)]);
    assert_eq!(verified.jtis()[0], credential.credential_id());
}
