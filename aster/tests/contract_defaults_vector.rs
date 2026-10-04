#![cfg(feature = "rpc")]
//! Golden vector for `#[aster(default = …)]` participation in contract
//! identity (Aster-ContractIdentity.md §11.3.2.3 "Default Values in
//! `contract_id`"), published as `conformance/vectors/contract-defaults.json`
//! so the Python and TypeScript producers can be brought in line with it.
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --features rpc --test contract_defaults_vector`

use std::collections::BTreeMap;
use std::path::PathBuf;

use aster::rpc::{contract_id, AsterType, MethodDef, MethodPattern, ScopeKind, ServiceContract};
use aster_transport_core::contract::canonical_xlang_bytes_type_def;
use fory_derive::ForyStruct;
use serde_json::{json, Value};

#[allow(dead_code)]
#[derive(ForyStruct, AsterType)]
#[aster(wire = "defaults.v1/Settings")]
struct Settings {
    /// required: no `#[aster(default)]`
    name: String,
    #[aster(default = "idle")]
    state: String,
    #[aster(default = "")]
    note: String,
    #[aster(default = true)]
    enabled: bool,
    #[aster(default = -3)]
    retries: i32,
    #[aster(default = 1_700_000_000_000i64)]
    since_ms: i64,
    #[aster(default = 0.5)]
    ratio: f64,
    #[aster(default = "x")]
    label: Option<String>,
    #[aster(default)]
    tags: Vec<String>,
    #[aster(default)]
    attrs: BTreeMap<String, String>,
    /// binary fields cannot carry a default in v1 → required
    #[fory(bytes)]
    blob: Vec<u8>,
}

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors/contract-defaults.json")
}

fn service() -> ServiceContract {
    let h = hex::encode(Settings::aster_type_hash());
    ServiceContract {
        name: "Defaults".into(),
        version: 1,
        methods: vec![MethodDef {
            name: "put".into(),
            pattern: MethodPattern::Unary,
            request_type: h.clone(),
            response_type: h,
            idempotent: false,
            default_timeout: 0.0,
            requires: None,
        }],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    }
}

fn actual() -> Value {
    let td = Settings::aster_type_def();
    let canonical = canonical_xlang_bytes_type_def(&td);
    let fields: Vec<Value> = td
        .fields
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "type_primitive": f.type_primitive,
                "optional": f.optional,
                "container": f.container as u8,
                "required": f.required,
                "default_value_hex": f.default_value,
            })
        })
        .collect();
    json!({
        "type_def": {
            "package": td.package,
            "name": td.name,
            "fields_in_declaration_order": fields,
        },
        "expected_canonical_type_def_hex": hex::encode(&canonical),
        "expected_type_hash": hex::encode(Settings::aster_type_hash()),
        "service": "Defaults v1, one unary method `put` taking and returning Settings, serialization [\"xlang\"], SHARED",
        "expected_contract_id": contract_id(&service()),
    })
}

#[test]
fn contract_defaults_vector() {
    let got = actual();
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        let doc = json!({
            "description": "Contract-identity golden vector for declared field defaults (Aster-ContractIdentity.md §11.3.2.3, 'Default Values in contract_id'). Produced by the Rust #[derive(AsterType)] with #[aster(default = ...)] attributes.",
            "rules": {
                "required": "FieldDef.required (field 13) = false iff the producer declared a contract default; true otherwise",
                "default_value": "FieldDef.default_value (field 14): empty bytes when required; else the canonical encoding of the default (see spec table); 0x00 for an empty list/set/map",
                "field_ids": "unaffected by defaults: 1-based position in NFC-name-sorted order",
                "wire_payload": "defaults never change the Fory payload bytes; they only change type_hash / contract_id"
            },
            "rust_source": "aster/tests/contract_defaults_vector.rs",
            "vector": got,
        });
        std::fs::write(path(), serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(doc["vector"], got, "contract-defaults vector drifted");
}

#[test]
fn defaults_change_identity() {
    // A defaulted field and the same field without a default hash differently
    // (§11.3.2.3 rule 4), so the vector above is not trivially equal to the
    // all-required shape the Python producer emits today.
    #[allow(dead_code)]
    #[derive(ForyStruct, AsterType)]
    #[aster(wire = "defaults.v1/Settings")]
    struct Required {
        name: String,
    }
    #[allow(dead_code)]
    #[derive(ForyStruct, AsterType)]
    #[aster(wire = "defaults.v1/Settings")]
    struct Defaulted {
        #[aster(default = "")]
        name: String,
    }
    assert_ne!(Required::aster_type_hash(), Defaulted::aster_type_hash());
}
