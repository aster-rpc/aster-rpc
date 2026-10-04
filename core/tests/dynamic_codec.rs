//! Dynamic Fory codec (`aster_transport_core::dynamic`): compatible-mode
//! evolution, wire-driven decoding, JSON forms, contracts and error cases.
//! Byte-identity against typed Rust and pyfory for every shape is pinned by
//! `conformance/vectors/dynamic-payloads.json` (`aster/tests/dynamic_payload_vector.rs`).

use std::collections::BTreeMap;

use aster_transport_core::contract::{
    build_contract_collection, canonical_xlang_bytes_type_def, compute_type_hash, ContainerKind,
    EnumValueDef, FieldDef, MethodDef, MethodPattern, ScopeKind, ServiceContract, TypeDef,
    TypeDefKind, TypeKind, UnionVariantDef,
};
use aster_transport_core::dynamic::{
    decode_json, encode_json, DecodeOptions, DynContract, DynErrorKind, DynSchema, DynStruct,
    DynValue, EncodeOptions, JsonOptions, MissingFields, StringEncoding, UnknownFields,
};
use aster_transport_core::xlang::new_fory;
use fory_derive::ForyStruct;
use serde_json::json;

// ── TypeDef builders ──────────────────────────────────────────────────────────

fn field(name: &str, prim: &str) -> FieldDef {
    FieldDef {
        id: 0,
        name: name.into(),
        type_kind: TypeKind::Primitive,
        type_primitive: prim.into(),
        type_ref: String::new(),
        self_ref_name: String::new(),
        optional: false,
        ref_tracked: false,
        container: ContainerKind::None,
        container_key_kind: TypeKind::Primitive,
        container_key_primitive: String::new(),
        container_key_ref: String::new(),
        required: true,
        default_value: String::new(),
    }
}

fn optional(mut f: FieldDef) -> FieldDef {
    f.optional = true;
    f
}

fn list(mut f: FieldDef) -> FieldDef {
    f.container = ContainerKind::List;
    f
}

fn map_of(mut f: FieldDef, key_prim: &str) -> FieldDef {
    f.container = ContainerKind::Map;
    f.container_key_primitive = key_prim.into();
    f
}

fn ref_field(name: &str, target: &TypeDef) -> FieldDef {
    let mut f = field(name, "");
    f.type_kind = TypeKind::Ref;
    f.type_ref = hash_hex(target);
    f
}

fn hash_hex(td: &TypeDef) -> String {
    hex::encode(compute_type_hash(&canonical_xlang_bytes_type_def(td)))
}

fn message(package: &str, name: &str, fields: Vec<FieldDef>) -> TypeDef {
    TypeDef {
        kind: TypeDefKind::Message,
        package: package.into(),
        name: name.into(),
        fields,
        enum_values: vec![],
        union_variants: vec![],
    }
}

fn color() -> TypeDef {
    TypeDef {
        kind: TypeDefKind::Enum,
        package: "t".into(),
        name: "Color".into(),
        fields: vec![],
        enum_values: vec![
            EnumValueDef {
                name: "RED".into(),
                value: 10,
            },
            EnumValueDef {
                name: "GREEN".into(),
                value: 20,
            },
        ],
        union_variants: vec![],
    }
}

fn user_v1() -> TypeDef {
    message(
        "t",
        "User",
        vec![field("name", "string"), field("age", "varint32")],
    )
}

fn user_v2() -> TypeDef {
    let mut email = optional(field("email", "string"));
    email.required = false;
    let mut level = field("level", "varint64");
    level.required = false;
    // canonical zig-zag varint of 5
    level.default_value = "0a".into();
    message(
        "t",
        "User",
        vec![
            field("name", "string"),
            field("age", "varint32"),
            email,
            level,
            list(field("roles", "string")),
        ],
    )
}

fn obj(v: serde_json::Value) -> DynValue {
    DynValue::from_json(&v)
}

fn json_of(v: &DynValue) -> serde_json::Value {
    v.to_json(&JsonOptions::default())
}

// ── compatible-mode evolution ─────────────────────────────────────────────────

#[test]
fn newer_producer_extra_fields_are_kept() {
    let v2 = DynSchema::from_type_defs([user_v2()]).unwrap();
    let v1 = DynSchema::from_type_defs([user_v1()]).unwrap();
    let bytes = v2
        .encode(
            "t.User",
            &obj(
                json!({"name": "ann", "age": 30, "email": "a@x", "level": "7", "roles": ["admin"]}),
            ),
        )
        .unwrap();
    let got = v1.decode_as("t.User", &bytes).unwrap();
    assert_eq!(
        json_of(&got),
        json!({"name": "ann", "age": 30, "email": "a@x", "level": "7", "roles": ["admin"]})
    );
    // Declared fields first, then the sender-only ones.
    let s = got.as_struct().unwrap();
    let names: Vec<&str> = s.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(&names[..2], &["name", "age"]);

    // Dropping unknown fields on request.
    let opts = DecodeOptions {
        keep_unknown_fields: false,
        ..DecodeOptions::default()
    };
    let got = v1.decode_with(Some("t.User"), &bytes, &opts).unwrap();
    assert_eq!(json_of(&got), json!({"name": "ann", "age": 30}));
}

#[test]
fn older_producer_missing_fields_are_defaulted() {
    let v1 = DynSchema::from_type_defs([user_v1()]).unwrap();
    let v2 = DynSchema::from_type_defs([user_v2()]).unwrap();
    let bytes = v1
        .encode("t.User", &obj(json!({"name": "bob", "age": 1})))
        .unwrap();
    let got = v2.decode_as("t.User", &bytes).unwrap();
    // email: optional -> null; level: declared default 5; roles: zero value.
    assert_eq!(
        json_of(&got),
        json!({"name": "bob", "age": 1, "email": null, "level": "5", "roles": []})
    );
    let opts = DecodeOptions {
        fill_missing: false,
        ..DecodeOptions::default()
    };
    let got = v2.decode_with(Some("t.User"), &bytes, &opts).unwrap();
    assert_eq!(json_of(&got), json!({"name": "bob", "age": 1}));
}

#[derive(ForyStruct, Debug, Default, PartialEq)]
struct UserV1 {
    name: String,
    age: i32,
}

#[derive(ForyStruct, Debug, Default, PartialEq)]
struct UserV2 {
    name: String,
    age: i32,
    email: Option<String>,
    level: i64,
    roles: Vec<String>,
}

#[test]
fn typed_readers_accept_dynamic_bytes_across_versions() {
    let v2 = DynSchema::from_type_defs([user_v2()]).unwrap();
    let bytes = v2
        .encode(
            "t.User",
            &obj(json!({"name": "c", "age": 2, "email": "e", "level": 9, "roles": ["r"]})),
        )
        .unwrap();
    let mut f1 = new_fory();
    f1.register_by_name::<UserV1>("t.User").unwrap();
    let u: UserV1 = f1.deserialize(&bytes).unwrap();
    assert_eq!(
        u,
        UserV1 {
            name: "c".into(),
            age: 2
        }
    );
    let mut f2 = new_fory();
    f2.register_by_name::<UserV2>("t.User").unwrap();
    let u: UserV2 = f2.deserialize(&bytes).unwrap();
    assert_eq!(u.level, 9);
    assert_eq!(
        f2.serialize(&u).unwrap(),
        bytes,
        "typed v2 writes the same bytes"
    );
}

// ── wire-driven decoding ──────────────────────────────────────────────────────

#[derive(ForyStruct, Debug, Default, PartialEq)]
struct Loose {
    m: BTreeMap<String, Option<String>>,
    l: Vec<Option<i32>>,
    s: Option<String>,
}

#[test]
fn decodes_without_a_local_schema() {
    let mut f = new_fory();
    f.register_by_name::<Loose>("w.Loose").unwrap();
    let v = Loose {
        m: [
            ("a".to_string(), Some("x".to_string())),
            ("b".to_string(), None),
        ]
        .into(),
        l: vec![Some(1), None, Some(-3)],
        s: None,
    };
    let bytes = f.serialize(&v).unwrap();
    let empty = DynSchema::from_type_defs(Vec::<TypeDef>::new()).unwrap();
    let got = empty.decode(&bytes).unwrap();
    let DynValue::Struct(s) = &got else {
        panic!("{got:?}")
    };
    assert_eq!(s.type_name, "w.Loose");
    assert_eq!(
        json_of(&got),
        json!({"l": [1, null, -3], "m": {"a": "x", "b": null}, "s": null})
    );
    // A root-type check against a type the schema lacks is an error.
    let err = empty.decode_as("w.Loose", &bytes).unwrap_err();
    assert_eq!(err.kind, DynErrorKind::UnknownType);
}

#[test]
fn enum_names_need_the_local_schema() {
    let c = color();
    let paint = message("t", "Paint", vec![ref_field("c", &c)]);
    let schema = DynSchema::from_type_defs([c.clone(), paint.clone()]).unwrap();
    let bytes = schema
        .encode("t.Paint", &obj(json!({"c": "GREEN"})))
        .unwrap();
    assert_eq!(
        json_of(&schema.decode(&bytes).unwrap()),
        json!({"c": "GREEN"})
    );
    // Ordinal input works too; the wire carries EnumValueDef.value.
    assert_eq!(
        schema.encode("t.Paint", &obj(json!({"c": 20}))).unwrap(),
        bytes
    );
    let empty = DynSchema::from_type_defs(Vec::<TypeDef>::new()).unwrap();
    assert_eq!(json_of(&empty.decode(&bytes).unwrap()), json!({"c": 20}));
}

// ── recursion, unions, JSON input forms ───────────────────────────────────────

/// Recursive types go through `SelfRef`. Typed Fory Rust 1.7.6 overflows the
/// stack serializing `Option<Box<Self>>`, so the oracle here is pyfory:
/// `@dataclass class Node: value: pyfory.Int32; next: Optional["Node"]`
/// registered as `r.Node`, serializing `Node(1, Node(2, None))`.
#[test]
fn self_referencing_type_matches_pyfory() {
    const PYFORY: &str = "01ff1e0012305927e959b61ce205440f35c3204c05d40ba1004a1e34979802ff1e0104fd";
    let mut next = optional(field("next", ""));
    next.type_kind = TypeKind::SelfRef;
    next.self_ref_name = "r.Node".into();
    let node = message("r", "Node", vec![field("value", "varint32"), next]);
    let schema = DynSchema::from_type_defs([node]).unwrap();
    let dynamic = schema
        .encode("r.Node", &obj(json!({"value": 1, "next": {"value": 2}})))
        .unwrap();
    assert_eq!(hex::encode(&dynamic), PYFORY);
    assert_eq!(
        json_of(&schema.decode(&dynamic).unwrap()),
        json!({"value": 1, "next": {"value": 2, "next": null}})
    );
}

fn union_schema() -> DynSchema {
    let a = message("u", "A", vec![field("x", "varint32")]);
    let b = message("u", "B", vec![field("y", "string")]);
    let un = TypeDef {
        kind: TypeDefKind::Union,
        package: "u".into(),
        name: "AB".into(),
        fields: vec![],
        enum_values: vec![],
        union_variants: vec![
            UnionVariantDef {
                name: "First".into(),
                id: 0,
                type_ref: hash_hex(&a),
            },
            UnionVariantDef {
                name: "Second".into(),
                id: 1,
                type_ref: hash_hex(&b),
            },
        ],
    };
    let holder = message("u", "Holder", vec![ref_field("v", &un)]);
    DynSchema::from_type_defs([a, b, un, holder]).unwrap()
}

#[test]
fn union_input_forms_agree() {
    let s = union_schema();
    let canonical = s
        .encode("u.Holder", &obj(json!({"v": {"_type": "u.B", "y": "hi"}})))
        .unwrap();
    for form in [
        json!({"v": {"_type": "Second", "y": "hi"}}),
        json!({"v": {"_type": "u/B", "y": "hi"}}),
        json!({"v": {"Second": {"y": "hi"}}}),
    ] {
        assert_eq!(
            s.encode("u.Holder", &obj(form.clone())).unwrap(),
            canonical,
            "{form}"
        );
    }
    let back = s.decode(&canonical).unwrap();
    assert_eq!(json_of(&back), json!({"v": {"_type": "u.B", "y": "hi"}}));
    let DynValue::Struct(st) = back else { panic!() };
    let DynValue::Union(u) = &st.fields[0].1 else {
        panic!()
    };
    assert_eq!(u.case_name.as_deref(), Some("Second"));
    assert_eq!(u.case_id, 1);
}

#[test]
fn json_input_leniency() {
    let td = message(
        "j",
        "All",
        vec![
            field("big", "varint64"),
            field("u", "var_uint64"),
            field("f", "float64"),
            field("blob", "binary"),
            map_of(field("by_id", "string"), "varint32"),
            field("when", "timestamp"),
            field("day", "date"),
        ],
    );
    let schema = DynSchema::from_type_defs([td]).unwrap();
    let canonical = schema
        .encode(
            "j.All",
            &obj(json!({
                "big": "-9007199254740993", "u": "18446744073709551615", "f": 0.1,
                "blob": "AQID_w", "by_id": [[1, "one"]],
                "when": "2023-11-14T22:13:20.123456Z", "day": "2024-02-29"
            })),
        )
        .unwrap();
    let alt = schema
        .encode(
            "j.All",
            &obj(json!({
                "big": -9007199254740993i64, "u": 18446744073709551615u64, "f": "0.1",
                "blob": [1, 2, 3, 255], "by_id": {"1": "one"},
                "when": "2023-11-14T23:13:20.123456+01:00", "day": "2024-02-29"
            })),
        )
        .unwrap();
    assert_eq!(canonical, alt);
    assert_eq!(
        json_of(&schema.decode(&canonical).unwrap()),
        json!({
            "big": "-9007199254740993", "u": "18446744073709551615", "f": 0.1,
            "blob": "AQID_w", "by_id": [[1, "one"]],
            "when": "2023-11-14T22:13:20.123456Z", "day": "2024-02-29"
        })
    );
    // Plain numbers on request.
    let plain = JsonOptions {
        int64_as_string: false,
        base64url: false,
    };
    let v = schema.decode(&canonical).unwrap().to_json(&plain);
    assert_eq!(v["big"], json!(-9007199254740993i64));
    assert_eq!(v["blob"], json!("AQID/w=="));
}

#[test]
fn string_encodings_all_decode() {
    let schema = DynSchema::from_type_defs([user_v1()]).unwrap();
    for s in ["plain", "caf\u{e9}", "\u{65e5}\u{672c}", "a\u{1F600}b", ""] {
        let v = obj(json!({"name": s, "age": 1}));
        let mut seen = Vec::new();
        for enc in [
            StringEncoding::Utf8,
            StringEncoding::Latin1OrUtf16,
            StringEncoding::Pyfory,
        ] {
            let opts = EncodeOptions {
                string_encoding: enc,
                ..EncodeOptions::default()
            };
            let bytes = schema.encode_with("t.User", &v, &opts).unwrap();
            assert_eq!(json_of(&schema.decode(&bytes).unwrap())["name"], json!(s));
            seen.push(bytes);
        }
        if s == "plain" {
            assert_ne!(seen[0], seen[1], "ASCII: UTF-8 vs LATIN1 header");
            assert_eq!(seen[1], seen[2]);
        }
    }
}

// ── contracts and the bytes/JSON entry points ─────────────────────────────────

fn echo_contract() -> (ServiceContract, Vec<TypeDef>) {
    let req = user_v1();
    let resp = user_v2();
    let resp = message("t", "Reply", resp.fields);
    let sc = ServiceContract {
        name: "Users".into(),
        version: 2,
        methods: vec![
            MethodDef {
                name: "get".into(),
                pattern: MethodPattern::Unary,
                request_type: hash_hex(&req),
                response_type: hash_hex(&resp),
                idempotent: true,
                default_timeout: 5.0,
                requires: None,
            },
            MethodDef {
                name: "watch".into(),
                pattern: MethodPattern::ServerStream,
                request_type: hash_hex(&req),
                response_type: hash_hex(&resp),
                idempotent: false,
                default_timeout: 0.0,
                requires: None,
            },
        ],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    };
    (sc, vec![req, resp])
}

#[test]
fn contract_from_collection_and_manifest() {
    let (sc, defs) = echo_contract();
    let entries = build_contract_collection(&sc, &defs, "me", 1).unwrap();
    let c = DynContract::from_collection(&entries).unwrap();
    assert_eq!(c.service, "Users");
    assert_eq!(c.version, 2);
    let get = c.method("get").unwrap();
    assert_eq!(get.request_type, "t.User");
    assert_eq!(get.response_type, "t.Reply");
    assert_eq!(
        c.method("watch").unwrap().pattern,
        MethodPattern::ServerStream
    );

    let manifest_json = &entries
        .iter()
        .find(|(n, _)| n == "manifest.json")
        .unwrap()
        .1;
    let types_json = serde_json::to_string(&defs).unwrap();
    let c2 =
        DynContract::from_json(std::str::from_utf8(manifest_json).unwrap(), &types_json).unwrap();
    assert_eq!(c2.contract_id, c.contract_id);
    assert_eq!(c2.method("get").unwrap().response_type, "t.Reply");

    let req = c
        .encode_request_json("get", br#"{"name":"z","age":3}"#)
        .unwrap();
    assert_eq!(
        c.decode_request("get", &req).unwrap(),
        DynValue::Struct(DynStruct {
            type_name: "t.User".into(),
            // Canonical TypeDef bytes list fields by name.
            fields: vec![
                ("age".into(), DynValue::I32(3)),
                ("name".into(), DynValue::String("z".into())),
            ],
        })
    );
    let resp = c
        .encode_response("get", &obj(json!({"name": "z", "age": 3, "roles": ["a"]})))
        .unwrap();
    let out: serde_json::Value =
        serde_json::from_slice(&c.decode_response_json("get", &resp).unwrap()).unwrap();
    assert_eq!(
        out,
        json!({"name": "z", "age": 3, "email": null, "level": "5", "roles": ["a"]})
    );
    // A response is not a request.
    assert!(c.decode_request("get", &resp).is_err());
    assert_eq!(
        c.method("nope").unwrap_err().kind,
        DynErrorKind::UnknownType
    );

    // A tampered type blob is refused.
    let mut bad = entries.clone();
    for (n, b) in bad.iter_mut() {
        if n.starts_with("types/") {
            b.push(0);
        }
    }
    assert!(DynContract::from_collection(&bad).is_err());
}

#[test]
fn one_shot_json_functions() {
    let types = serde_json::to_vec(&vec![user_v1()]).unwrap();
    let bytes = encode_json(&types, "t/User", br#"{"name":"q","age":-1}"#).unwrap();
    let back = decode_json(&types, Some("User"), &bytes).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&back).unwrap(),
        json!({"name": "q", "age": -1})
    );
    // `{"types": [...]}` is accepted as well.
    let wrapped = serde_json::to_vec(&json!({"types": [user_v1()]})).unwrap();
    assert_eq!(
        encode_json(&wrapped, "t.User", br#"{"name":"q","age":-1}"#).unwrap(),
        bytes
    );
}

// ── error cases ───────────────────────────────────────────────────────────────

fn kind_of<T: std::fmt::Debug>(
    r: Result<T, aster_transport_core::dynamic::DynError>,
) -> DynErrorKind {
    r.unwrap_err().kind
}

#[test]
fn encode_errors() {
    let schema = DynSchema::from_type_defs([user_v2(), color()]).unwrap();
    let enc = |v: serde_json::Value| schema.encode("t.User", &obj(v));
    assert_eq!(
        kind_of(schema.encode("t.Nope", &DynValue::Null)),
        DynErrorKind::UnknownType
    );
    assert_eq!(
        kind_of(enc(json!({"name": "a", "agee": 1}))),
        DynErrorKind::InvalidValue
    );
    assert_eq!(
        kind_of(enc(json!({"name": "a", "age": 2147483648i64}))),
        DynErrorKind::InvalidValue
    );
    assert_eq!(kind_of(enc(json!({"name": 5}))), DynErrorKind::InvalidValue);
    assert_eq!(
        kind_of(enc(json!({"name": null}))),
        DynErrorKind::InvalidValue
    );
    assert_eq!(
        kind_of(enc(json!({"name": "a", "roles": "x"}))),
        DynErrorKind::InvalidValue
    );
    assert_eq!(kind_of(enc(json!([1, 2]))), DynErrorKind::InvalidValue);
    assert_eq!(
        kind_of(schema.encode("t.Color", &obj(json!("BLUE")))),
        DynErrorKind::InvalidValue
    );
    // Errors name the path.
    let err = enc(json!({"name": "a", "roles": ["ok", 7]})).unwrap_err();
    assert_eq!(err.path, "t.User.roles[1]");

    // Unknown fields can be ignored; required fields can be enforced.
    let lenient = EncodeOptions {
        unknown_fields: UnknownFields::Ignore,
        ..EncodeOptions::default()
    };
    assert_eq!(
        schema
            .encode_with("t.User", &obj(json!({"name": "a", "zzz": 1})), &lenient)
            .unwrap(),
        enc(json!({"name": "a"})).unwrap()
    );
    let strict = EncodeOptions {
        missing_fields: MissingFields::Error,
        ..EncodeOptions::default()
    };
    let err = schema
        .encode_with("t.User", &obj(json!({"name": "a"})), &strict)
        .unwrap_err();
    assert!(err.message.contains("missing required field age"), "{err}");
    // Optional / defaulted fields are still filled under the strict policy.
    schema
        .encode_with(
            "t.User",
            &obj(json!({"name": "a", "age": 1, "roles": []})),
            &strict,
        )
        .unwrap();
}

#[test]
fn decode_errors() {
    let schema = DynSchema::from_type_defs([user_v1(), color()]).unwrap();
    let good = schema
        .encode("t.User", &obj(json!({"name": "abc", "age": 1})))
        .unwrap();
    // Truncated anywhere.
    for n in 0..good.len() {
        assert!(schema.decode(&good[..n]).is_err(), "truncated at {n}");
    }
    // Trailing bytes.
    let mut long = good.clone();
    long.push(0);
    assert_eq!(kind_of(schema.decode(&long)), DynErrorKind::Decode);
    // Not xlang.
    let mut native = good.clone();
    native[0] = 0;
    assert_eq!(kind_of(schema.decode(&native)), DynErrorKind::Decode);
    // Corrupted TypeDef body (hash check).
    let mut corrupt = good.clone();
    corrupt[14] ^= 0x01;
    assert!(schema.decode(&corrupt).is_err());
    // Wrong root type.
    assert_eq!(
        kind_of(schema.decode_as("t.Color", &good)),
        DynErrorKind::Decode
    );
    // Null root.
    assert_eq!(schema.decode(&[1, 0xfd]).unwrap(), DynValue::Null);
}

#[test]
fn schema_errors() {
    let mut any = field("x", "");
    any.type_kind = TypeKind::Any;
    assert_eq!(
        kind_of(DynSchema::from_type_defs([message("s", "A", vec![any])])),
        DynErrorKind::Unsupported
    );
    assert_eq!(
        kind_of(DynSchema::from_type_defs([message(
            "s",
            "U",
            vec![field("id", "uuid")]
        )])),
        DynErrorKind::Unsupported
    );
    let dangling = ref_field("x", &user_v1());
    assert_eq!(
        kind_of(DynSchema::from_type_defs([message(
            "s",
            "D",
            vec![dangling]
        )])),
        DynErrorKind::Schema
    );
    // Two different types with one wire name.
    assert_eq!(
        kind_of(DynSchema::from_type_defs([user_v1(), user_v2()])),
        DynErrorKind::Schema
    );
    assert_eq!(kind_of(DynSchema::from_json("{}")), DynErrorKind::Schema);
}

#[test]
fn lookup_forms() {
    let schema = DynSchema::from_type_defs([user_v1()]).unwrap();
    let h = hash_hex(&user_v1());
    for n in ["t.User", "t/User", "User", h.as_str(), &h.to_uppercase()] {
        assert_eq!(schema.resolve(n), Some("t.User"), "{n}");
    }
    assert_eq!(schema.resolve("aster/Empty"), Some("aster.Empty"));
    assert_eq!(
        schema.wire_field_order("t.User").unwrap(),
        vec!["age", "name"],
        "primitives before strings"
    );
    assert!(schema.type_meta_bytes("t.User").is_some());
    // The implicit empty request encodes like a typed empty struct.
    let empty = schema.encode("aster.Empty", &obj(json!({}))).unwrap();
    assert_eq!(json_of(&schema.decode(&empty).unwrap()), json!({}));
}
