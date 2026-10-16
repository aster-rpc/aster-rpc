//! The dynamic codec against `conformance/vectors/contract-types.json`: the
//! canonical `timestamp` / `date` / `decimal` / `decimal(p,s)` / `uuid` /
//! `ulid` types and recursive types, as the typed Rust derive writes them
//! (`aster/tests/it/contract_types.rs` produces the file).
//!
//! Per type: the schema built from the published canonical `TypeDef` bytes
//! has the recorded hash. Per payload: decoding the typed bytes gives the
//! recorded JSON value, and encoding that value reproduces the bytes. Per
//! decode case: the decoder refuses what the spec says to refuse (wrong-length
//! identifiers, a decimal that needs rounding or has too many digits) and
//! rescales what it accepts.

use std::path::PathBuf;

use aster_transport_core::contract::decode_type_def_bytes;
use aster_transport_core::dynamic::{DynErrorKind, DynSchema, DynValue, JsonOptions};
use serde_json::{json, Value};

fn vectors() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/vectors/contract-types.json");
    serde_json::from_slice(&std::fs::read(&path).expect("read contract-types.json"))
        .expect("contract-types.json is JSON")
}

fn hex_of(v: &Value) -> Vec<u8> {
    hex::decode(v.as_str().expect("hex string")).expect("hex")
}

fn schema(vector: &Value) -> DynSchema {
    let defs: Vec<Vec<u8>> = vector["types"]
        .as_object()
        .unwrap()
        .values()
        .map(|t| hex_of(&t["expected_canonical_type_def_hex"]))
        .collect();
    DynSchema::from_canonical_type_defs(defs.iter().map(Vec::as_slice)).expect("schema")
}

#[test]
fn schema_hashes_match_the_typed_derive() {
    let v = vectors();
    let vector = &v["vector"];
    let schema = schema(vector);
    for (name, t) in vector["types"].as_object().unwrap() {
        let bytes = hex_of(&t["expected_canonical_type_def_hex"]);
        let def = decode_type_def_bytes(&bytes).unwrap();
        assert_eq!(format!("{}.{}", def.package, def.name), *name);
        assert_eq!(
            hex::encode(schema.type_hash(name).expect(name)),
            t["expected_type_hash"].as_str().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn payloads_decode_to_the_recorded_values_and_encode_back_to_the_same_bytes() {
    let v = vectors();
    let vector = &v["vector"];
    let schema = schema(vector);
    let opts = JsonOptions::default();
    let payloads = vector["payloads"].as_object().unwrap();
    assert!(payloads.len() >= 6, "payload vectors present");
    for (case, p) in payloads {
        let ty = p["type"].as_str().unwrap();
        let bytes = hex_of(&p["hex"]);
        let decoded = schema
            .decode_as(ty, &bytes)
            .unwrap_or_else(|e| panic!("{case}: decode: {e}"));
        assert_eq!(decoded.to_json(&opts), p["value"], "{case}: decoded value");
        let encoded = schema
            .encode(ty, &DynValue::from_json(&p["value"]))
            .unwrap_or_else(|e| panic!("{case}: encode: {e}"));
        assert_eq!(
            hex::encode(&encoded),
            p["hex"].as_str().unwrap(),
            "{case}: dynamic bytes differ from the typed derive's"
        );
    }
}

#[test]
fn decode_cases_refuse_and_rescale_as_the_spec_says() {
    let v = vectors();
    let vector = &v["vector"];
    let schema = schema(vector);
    let cases = vector["decode_cases"].as_object().unwrap();
    assert!(cases.len() >= 6, "decode cases present");
    for (case, c) in cases {
        let ty = c["type"].as_str().unwrap();
        let got = schema.decode_as(ty, &hex_of(&c["hex"]));
        if c["refused"].as_bool().unwrap() {
            let err = got.expect_err(case);
            assert_eq!(err.kind, DynErrorKind::Decode, "{case}: {err}");
        } else {
            let value = got.unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(value.to_json(&JsonOptions::default()), c["value"], "{case}");
        }
    }
}

#[test]
fn encoder_refuses_what_does_not_fit_and_pads_what_does() {
    let v = vectors();
    let schema = schema(&v["vector"]);
    let priced = |price: &str, rate: &str| {
        DynValue::from_json(&json!({
            "price": price, "rate": rate, "maybe_price": null, "amount": "0"
        }))
    };
    for (price, rate) in [("1.23456", "1"), ("123456789012345", "1"), ("1", "123456")] {
        let err = schema
            .encode("types.v1.Priced", &priced(price, rate))
            .expect_err(price);
        assert_eq!(err.kind, DynErrorKind::InvalidValue, "{err}");
    }
    let bytes = schema
        .encode("types.v1.Priced", &priced("1.5", "7.0"))
        .unwrap();
    let back = schema.decode_as("types.v1.Priced", &bytes).unwrap();
    assert_eq!(back.to_json(&JsonOptions::default())["price"], "1.5000");
    assert_eq!(back.to_json(&JsonOptions::default())["rate"], "7");

    let identified = |id: Value| {
        DynValue::from_json(&json!({
            "id": id, "trace": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "parent": null,
            "history": [], "names": {}
        }))
    };
    assert!(schema
        .encode("types.v1.Identified", &identified(json!("not-a-uuid")))
        .is_err());
    // A ULID string is not a UUID: the two are distinct contract types.
    assert!(schema
        .encode(
            "types.v1.Identified",
            &identified(json!("01ARZ3NDEKTSV4RRFFQ69G5FAV"))
        )
        .is_err());
    assert!(schema
        .encode(
            "types.v1.Identified",
            &identified(json!("0190F2A8-1B2C-7D3E-8F40-123456789ABC"))
        )
        .is_ok());
}

#[test]
fn malformed_decimal_params_are_a_schema_error() {
    let v = vectors();
    let priced =
        hex_of(&v["vector"]["types"]["types.v1.Priced"]["expected_canonical_type_def_hex"]);
    let def = decode_type_def_bytes(&priced).unwrap();
    assert!(DynSchema::from_type_defs([def.clone()]).is_ok());
    for bad in [
        "decimal(18, 4)",
        "decimal(018,4)",
        "decimal(4,5)",
        "decimal(0,0)",
    ] {
        let mut d = def.clone();
        let f = d
            .fields
            .iter_mut()
            .find(|f| f.type_primitive == "decimal(18,4)")
            .unwrap();
        f.type_primitive = bad.into();
        let err = DynSchema::from_type_defs([d]).unwrap_err();
        assert_eq!(err.kind, DynErrorKind::Schema, "{bad}: {err}");
        assert!(err.message.contains("decimal(p,s)"), "{bad}: {err}");
    }
}
