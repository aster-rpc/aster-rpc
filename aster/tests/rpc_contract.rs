#![cfg(feature = "rpc")]
//! Cross-binding contract-identity verification for `#[derive(AsterType)]`.
//!
//! The golden values are produced by the repo's reference producer
//! `scripts/cross_lang_echo_contract_id.py` (run: `uv run python
//! scripts/cross_lang_echo_contract_id.py --debug`). Matching them proves the
//! Rust derive emits TypeDefs whose canonical bytes / BLAKE3 hashes are
//! byte-identical to the other bindings — i.e. contract-ids interoperate.
//!
//! Note: the Python reference emits `required=true` for fields even when the
//! dataclass declares a default (`message: str = ""`), so the Rust types here
//! are declared WITHOUT `#[aster(default = ...)]` to match. The spec-default
//! lowering (which Rust implements ahead of the other bindings) is exercised
//! separately in `default_lowers_into_contract`.

use aster::rpc::{contract_id, AsterType, MethodDef, MethodPattern, ScopeKind, ServiceContract};

#[derive(AsterType)]
#[aster(wire = "echo/EchoRequest")]
struct EchoRequest {
    #[allow(dead_code)]
    message: String,
}

#[derive(AsterType)]
#[aster(wire = "echo/EchoResponse")]
struct EchoResponse {
    #[allow(dead_code)]
    reply: String,
}

#[test]
fn echo_typedef_hashes_match_python_golden() {
    assert_eq!(
        EchoRequest::aster_type_hash_hex(),
        "4a2fa9b8f8cfbd325d72dc9739e416c86cd3cd5724882f22400ab08d2db49dd6",
        "EchoRequest TypeDef hash diverged from the cross-binding golden"
    );
    assert_eq!(
        EchoResponse::aster_type_hash_hex(),
        "ef84ecadec2481a55fc7b5b0d011f65814c61441ede296c907dde9a367d2a18f",
        "EchoResponse TypeDef hash diverged from the cross-binding golden"
    );
}

#[test]
fn echo_service_contract_id_matches_python_golden() {
    let sc = ServiceContract {
        name: "EchoService".into(),
        version: 1,
        methods: vec![MethodDef {
            name: "echo".into(),
            pattern: MethodPattern::Unary,
            request_type: EchoRequest::aster_type_hash_hex(),
            response_type: EchoResponse::aster_type_hash_hex(),
            idempotent: false,
            default_timeout: 0.0,
            requires: None,
        }],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    };
    assert_eq!(
        contract_id(&sc),
        "12d2f2990f4dd71dfd59f5db470d186f1fcc7dbafdac0ea7fdf838ab263c0578",
        "EchoService contract_id diverged from the cross-binding golden"
    );
}

// ── Field-mapping coverage (Rust-internal) ───────────────────────────────────

#[derive(AsterType)]
#[aster(wire = "t/Shapes")]
#[allow(dead_code)]
struct Shapes {
    tags: Vec<String>,
    blob: Vec<u8>,
    maybe: Option<String>,
    nested: EchoRequest,
}

#[test]
fn container_optional_binary_and_ref_mapping() {
    let td = Shapes::aster_type_def();
    let f = |name: &str| td.fields.iter().find(|f| f.name == name).unwrap().clone();

    let tags = f("tags");
    assert_eq!(tags.type_primitive, "string");
    assert_eq!(format!("{:?}", tags.container), "List");
    assert!(!tags.optional);

    let blob = f("blob");
    assert_eq!(blob.type_primitive, "binary");
    assert_eq!(format!("{:?}", blob.container), "None");

    let maybe = f("maybe");
    assert_eq!(maybe.type_primitive, "string");
    assert!(maybe.optional);

    let nested = f("nested");
    assert_eq!(format!("{:?}", nested.type_kind), "Ref");
    // Ref target is the referenced type's hash.
    assert_eq!(nested.type_ref, EchoRequest::aster_type_hash_hex());
}

// ── Spec-default lowering (§11.3.2.3) ─────────────────────────────────────────

#[derive(AsterType)]
#[aster(wire = "t/WithDefaults")]
#[allow(dead_code)]
struct WithDefaults {
    #[aster(default = "idle")]
    state: String,
    #[aster(default = 7)]
    retries: i32,
    plain: i64,
}

#[test]
fn default_lowers_into_contract() {
    let td = WithDefaults::aster_type_def();
    let f = |name: &str| td.fields.iter().find(|f| f.name == name).unwrap().clone();

    let state = f("state");
    assert!(!state.required, "defaulted field must be required=false");
    // canonical write_string("idle"): header (4<<2)|2 = 0x12, then "idle".
    assert_eq!(state.default_value, "1269646c65");

    let retries = f("retries");
    assert!(!retries.required);
    // canonical write_zigzag_i32(7): zigzag(7)=14=0x0e.
    assert_eq!(retries.default_value, "0e");
    assert_eq!(retries.type_primitive, "varint32");

    let plain = f("plain");
    assert!(
        plain.required,
        "field without a default must be required=true"
    );
    assert_eq!(plain.default_value, "");
    assert_eq!(plain.type_primitive, "varint64");
}
