#![cfg(feature = "rpc")]
//! Golden vectors for the dynamic Fory codec (`aster_transport_core::dynamic`,
//! plan D5), published as `conformance/vectors/dynamic-payloads.json`.
//!
//! Every `cases` entry is produced by the **typed** Rust path —
//! `#[derive(ForyStruct, AsterType)]` types serialized by a Fory 1.7.6 runtime
//! built with `new_payload_fory()` and `register_by_name("package.Name")` —
//! and the schema is the `TypeDef`s those same derives publish. The test then
//! proves, per case:
//!
//! 1. decode(typed bytes) → JSON equals the recorded `value`;
//! 2. encode(`value`) with only the schema reproduces the typed bytes exactly;
//! 3. the typed runtime reads the dynamic bytes back to the original value.
//!
//! `pyfory_cases` were produced by pyfory (see
//! `conformance/vectors/dynamic-payloads-pyfory.py`) for Python dataclasses
//! with the same wire names and fields: they must decode to `value`, and
//! encoding `value` with `StringEncoding::Pyfory` must reproduce them.
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --features rpc --test dynamic_payload_vector`
//! then re-run the pyfory script to refresh `pyfory_cases`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use aster::rpc::{new_payload_fory, AsterType, Leaf, TypeDef, WireField};
use aster_transport_core::contract::{
    canonical_xlang_bytes_type_def, compute_type_hash, ContainerKind, EnumValueDef, FieldDef,
    TypeDefKind, TypeKind,
};
use aster_transport_core::dynamic::{
    DynSchema, DynValue, EncodeOptions, JsonOptions, StringEncoding,
};
use fory_core::Fory;
use fory_derive::{ForyEnum, ForyStruct, ForyUnion};
use serde_json::{json, Value};

#[path = "../../tests/interop/rust/contract.rs"]
mod interop;
use interop::{EchoReply, EchoRequest};

// ── golden types ──────────────────────────────────────────────────────────────

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Inner")]
struct Inner {
    a: i32,
    s: String,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Scalars")]
struct Scalars {
    flag: bool,
    tiny: i8,
    small: i16,
    medium: i32,
    large: i64,
    ushort: u16,
    uint: u32,
    ulong: u64,
    ratio32: f32,
    ratio64: f64,
    text: String,
    #[fory(bytes)]
    blob: Vec<u8>,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Optionals")]
struct Optionals {
    maybe_int: Option<i32>,
    maybe_long: Option<i64>,
    maybe_flag: Option<bool>,
    maybe_ratio: Option<f64>,
    maybe_text: Option<String>,
    maybe_inner: Option<Inner>,
    maybe_list: Option<Vec<String>>,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Collections")]
struct Collections {
    names: Vec<String>,
    counts: Vec<i32>,
    longs: Vec<i64>,
    flags: Vec<bool>,
    ratios: Vec<f64>,
    inners: Vec<Inner>,
    tags: BTreeSet<String>,
    scores: BTreeMap<String, i32>,
    by_id: BTreeMap<i32, String>,
    nested: BTreeMap<String, Inner>,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Nested")]
struct Nested {
    first: Inner,
    second: Inner,
    more: Vec<Inner>,
    label: String,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/BigMap")]
struct BigMap {
    m: BTreeMap<i32, i32>,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Defaults")]
struct Defaults {
    name: String,
    #[aster(default = "idle")]
    state: String,
    #[aster(default = true)]
    enabled: bool,
    #[aster(default = -3)]
    retries: i32,
    #[aster(default = 1_700_000_000_000i64)]
    since_ms: i64,
    #[aster(default = 0.5)]
    ratio: f64,
    #[aster(default)]
    tags: Vec<String>,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/IntBox")]
struct IntBox {
    value: i64,
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/TextBox")]
struct TextBox {
    value: String,
}

#[derive(ForyUnion, AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Scalar")]
enum Scalar {
    #[fory(default)]
    Int(IntBox),
    Text(TextBox),
    #[fory(unknown)]
    Unknown(fory_core::UnknownCase),
}

#[derive(ForyStruct, AsterType, Debug, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Holder")]
struct Holder {
    u: Scalar,
    ou: Option<Scalar>,
    lu: Vec<Scalar>,
}

/// A plain enum. `#[derive(AsterType)]` covers structs and unions only, so the
/// contract `TypeDef` is written by hand (ordinal = declaration index, which
/// is what `ForyEnum` puts on the wire).
#[derive(ForyEnum, Debug, Default, Clone, Copy, PartialEq)]
enum Color {
    #[default]
    Red,
    Green,
    Blue,
}

fn color_type_def() -> TypeDef {
    TypeDef {
        kind: TypeDefKind::Enum,
        package: "dyn.v1".into(),
        name: "Color".into(),
        fields: vec![],
        enum_values: ["Red", "Green", "Blue"]
            .iter()
            .enumerate()
            .map(|(i, n)| EnumValueDef {
                name: n.to_string(),
                value: i as i32,
            })
            .collect(),
        union_variants: vec![],
    }
}

impl AsterType for Color {
    fn aster_type_def() -> TypeDef {
        color_type_def()
    }
}

impl WireField for Color {
    fn leaf() -> Leaf {
        Leaf::reference(compute_type_hash(&canonical_xlang_bytes_type_def(
            &color_type_def(),
        )))
    }
}

#[derive(ForyStruct, AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "dyn.v1/Paint")]
struct Paint {
    c: Color,
    oc: Option<Color>,
    lc: Vec<Color>,
}

/// Fixed-width and tagged integers: no Rust `AsterType` lowering (the derive
/// maps `i32` to `varint32`), so this type exists only as a hand-written
/// `TypeDef` here and as a pyfory dataclass (`pyfory_cases`).
fn fixed_type_def() -> TypeDef {
    let f = |name: &str, prim: &str| FieldDef {
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
    };
    TypeDef {
        kind: TypeDefKind::Message,
        package: "dyn.v1".into(),
        name: "Fixed".into(),
        fields: vec![
            f("fi32", "int32"),
            f("fi64", "int64"),
            f("fu32", "uint32"),
            f("fu64", "uint64"),
            f("ti64", "tagged_int64"),
            f("tu64", "tagged_uint64"),
            f("u8", "uint8"),
        ],
        enum_values: vec![],
        union_variants: vec![],
    }
}

// ── typed side ────────────────────────────────────────────────────────────────

fn type_defs() -> Vec<TypeDef> {
    vec![
        Inner::aster_type_def(),
        Scalars::aster_type_def(),
        Optionals::aster_type_def(),
        Collections::aster_type_def(),
        Nested::aster_type_def(),
        BigMap::aster_type_def(),
        Defaults::aster_type_def(),
        IntBox::aster_type_def(),
        TextBox::aster_type_def(),
        Scalar::aster_type_def(),
        Holder::aster_type_def(),
        color_type_def(),
        Paint::aster_type_def(),
        fixed_type_def(),
        EchoRequest::aster_type_def(),
        EchoReply::aster_type_def(),
    ]
}

fn typed_fory() -> Fory {
    let mut f = new_payload_fory();
    f.register_by_name::<Inner>("dyn.v1.Inner").unwrap();
    f.register_by_name::<Scalars>("dyn.v1.Scalars").unwrap();
    f.register_by_name::<Optionals>("dyn.v1.Optionals").unwrap();
    f.register_by_name::<Collections>("dyn.v1.Collections")
        .unwrap();
    f.register_by_name::<Nested>("dyn.v1.Nested").unwrap();
    f.register_by_name::<BigMap>("dyn.v1.BigMap").unwrap();
    f.register_by_name::<Defaults>("dyn.v1.Defaults").unwrap();
    f.register_by_name::<IntBox>("dyn.v1.IntBox").unwrap();
    f.register_by_name::<TextBox>("dyn.v1.TextBox").unwrap();
    f.register_union_by_name::<Scalar>("dyn.v1.Scalar").unwrap();
    f.register_by_name::<Holder>("dyn.v1.Holder").unwrap();
    f.register_by_name::<Color>("dyn.v1.Color").unwrap();
    f.register_by_name::<Paint>("dyn.v1.Paint").unwrap();
    f.register_by_name::<EchoRequest>("interop.EchoRequest")
        .unwrap();
    f.register_by_name::<EchoReply>("interop.EchoReply")
        .unwrap();
    f
}

/// Typed decoding of (dynamic) bytes gives the original value back.
type TypedCheck = Box<dyn Fn(&Fory, &[u8])>;

/// One typed case: its name, wire type, typed bytes, and its [`TypedCheck`].
struct Case {
    name: &'static str,
    type_name: &'static str,
    bytes: Vec<u8>,
    check: TypedCheck,
}

fn case<T>(f: &Fory, name: &'static str, type_name: &'static str, v: T) -> Case
where
    T: fory_core::Serializer<Target = T> + PartialEq + std::fmt::Debug + Clone + 'static,
{
    let bytes = f.serialize(&v).unwrap();
    Case {
        name,
        type_name,
        bytes,
        check: Box::new(move |f, b| {
            let back: T = f.deserialize(b).unwrap();
            assert_eq!(back, v, "{name}: typed decode of the dynamic bytes");
        }),
    }
}

fn inner(a: i32, s: &str) -> Inner {
    Inner { a, s: s.into() }
}

fn cases(f: &Fory) -> Vec<Case> {
    vec![
        case(f, "inner", "dyn.v1.Inner", inner(5, "hi")),
        case(f, "inner zero", "dyn.v1.Inner", Inner::default()),
        case(
            f,
            "scalars",
            "dyn.v1.Scalars",
            Scalars {
                flag: true,
                tiny: -1,
                small: 300,
                medium: -7,
                large: 1 << 40,
                ushort: 65_535,
                uint: 7,
                ulong: 1 << 50,
                ratio32: 1.5,
                ratio64: -2.25,
                text: "h\u{e9}llo \u{65e5}\u{672c} \u{1F600}".into(),
                blob: vec![0, 1, 2, 254, 255],
            },
        ),
        case(
            f,
            "scalars extremes",
            "dyn.v1.Scalars",
            Scalars {
                flag: false,
                tiny: i8::MIN,
                small: i16::MIN,
                medium: i32::MIN,
                large: i64::MIN,
                ushort: 0,
                uint: u32::MAX,
                ulong: u64::MAX,
                ratio32: f32::MAX,
                ratio64: f64::MIN_POSITIVE,
                text: String::new(),
                blob: vec![],
            },
        ),
        case(
            f,
            "optionals all null",
            "dyn.v1.Optionals",
            Optionals::default(),
        ),
        case(
            f,
            "optionals all set",
            "dyn.v1.Optionals",
            Optionals {
                maybe_int: Some(3),
                maybe_long: Some(-9_000_000_000),
                maybe_flag: Some(false),
                maybe_ratio: Some(0.1),
                maybe_text: Some("x".into()),
                maybe_inner: Some(inner(1, "y")),
                maybe_list: Some(vec!["q".into(), "r".into()]),
            },
        ),
        case(
            f,
            "collections",
            "dyn.v1.Collections",
            Collections {
                names: vec!["a".into(), "b".into()],
                counts: vec![1, -1, 300],
                longs: vec![5, -5_000_000_000],
                flags: vec![true, false],
                ratios: vec![1.0, -0.5],
                inners: vec![inner(1, ""), inner(2, "two")],
                tags: ["x", "y"].iter().map(|s| s.to_string()).collect(),
                scores: [("k".to_string(), 9), ("m".to_string(), -1)].into(),
                by_id: [(1, "one".to_string()), (-2, "minus two".to_string())].into(),
                nested: [("n".to_string(), inner(4, "four"))].into(),
            },
        ),
        case(
            f,
            "collections empty",
            "dyn.v1.Collections",
            Collections::default(),
        ),
        case(
            f,
            "nested struct metas are shared",
            "dyn.v1.Nested",
            Nested {
                first: inner(1, "a"),
                second: inner(2, "b"),
                more: vec![inner(3, "c")],
                label: "L".into(),
            },
        ),
        case(
            f,
            "map over one chunk (300 entries)",
            "dyn.v1.BigMap",
            BigMap {
                m: (0..300).map(|i| (i - 150, i * 3)).collect(),
            },
        ),
        case(
            f,
            "declared defaults",
            "dyn.v1.Defaults",
            Defaults {
                name: "n".into(),
                state: "idle".into(),
                enabled: true,
                retries: -3,
                since_ms: 1_700_000_000_000,
                ratio: 0.5,
                tags: vec![],
            },
        ),
        case(
            f,
            "union (int case)",
            "dyn.v1.Scalar",
            Scalar::Int(IntBox { value: 7 }),
        ),
        case(
            f,
            "union fields",
            "dyn.v1.Holder",
            Holder {
                u: Scalar::Text(TextBox { value: "t".into() }),
                ou: Some(Scalar::Int(IntBox { value: -1 })),
                lu: vec![
                    Scalar::Int(IntBox { value: 1 }),
                    Scalar::Text(TextBox {
                        value: "two".into(),
                    }),
                ],
            },
        ),
        case(
            f,
            "union fields, optional null",
            "dyn.v1.Holder",
            Holder {
                u: Scalar::Int(IntBox { value: 0 }),
                ou: None,
                lu: vec![],
            },
        ),
        case(
            f,
            "enum fields",
            "dyn.v1.Paint",
            Paint {
                c: Color::Blue,
                oc: Some(Color::Green),
                lc: vec![Color::Red, Color::Blue],
            },
        ),
        case(f, "enum root", "dyn.v1.Color", Color::Green),
        case(
            f,
            "interop EchoRequest",
            "interop.EchoRequest",
            EchoRequest {
                message: "hello".into(),
                count: 3,
                data: vec![1, 2, 3],
            },
        ),
        case(
            f,
            "interop EchoReply",
            "interop.EchoReply",
            EchoReply {
                message: "hello".into(),
                count: 4,
                data: vec![1, 2, 3],
                server_lang: "rust".into(),
            },
        ),
    ]
}

// ── vector file ───────────────────────────────────────────────────────────────

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors/dynamic-payloads.json")
}

fn schema() -> DynSchema {
    DynSchema::from_type_defs(type_defs()).expect("schema")
}

fn generated(f: &Fory, schema: &DynSchema) -> Value {
    let mut out = Vec::new();
    for c in cases(f) {
        let decoded = schema
            .decode_as(c.type_name, &c.bytes)
            .unwrap_or_else(|e| panic!("{}: decode: {e}", c.name));
        let value = decoded.to_json(&JsonOptions::default());
        out.push(json!({
            "name": c.name,
            "type": c.type_name,
            "value": value,
            "hex": hex::encode(&c.bytes),
        }));
    }
    Value::Array(out)
}

fn load() -> Value {
    serde_json::from_str(&std::fs::read_to_string(path()).expect("vector file")).unwrap()
}

#[test]
fn dynamic_payload_vectors() {
    let f = typed_fory();
    let schema = schema();
    let types: Value = serde_json::to_value(type_defs()).unwrap();
    let got_cases = generated(&f, &schema);
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        let pyfory_cases = std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .map(|d| d["pyfory_cases"].clone())
            .filter(|v| v.is_array())
            .unwrap_or_else(|| json!([]));
        let doc = json!({
            "description": "Dynamic Fory codec golden vectors (plan D5). Payloads are Fory 1.7.6 xlang with xlang=true, compatible=true, ref=false. `types` is the schema (contract TypeDefs, JSON). Each case: `hex` = the payload of `value` as the given `type`; `value` uses the Aster JSON encoding (int64/uint64 as decimal strings, binary as unpadded base64url, enums by name, unions as {\"_type\": <payload type>, ...}, maps with string keys as objects, other maps as [[key, value], ...]). A conforming dynamic codec must (1) decode `hex` to `value` and (2) encode `value` to exactly `hex` (cases: UTF-8 strings, as Fory Rust writes them; pyfory_cases: pyfory string encoding - LATIN1, else UTF-16 for BMP text, else UTF-8).",
            "fory_version": "1.7.6",
            "options": {"xlang": true, "compatible": true, "ref": false},
            "rust_source": "aster/tests/dynamic_payload_vector.rs",
            "pyfory_source": "conformance/vectors/dynamic-payloads-pyfory.py",
            "types": types,
            "cases": got_cases,
            "pyfory_cases": pyfory_cases,
        });
        std::fs::write(path(), serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }
    let doc = load();
    assert_eq!(doc["types"], types, "dynamic-payloads types drifted");
    assert_eq!(doc["cases"], got_cases, "dynamic-payloads cases drifted");
}

/// The core proof: schema-only encoding reproduces the typed bytes, and the
/// typed runtime reads the dynamic bytes.
#[test]
fn dynamic_encoding_matches_typed_rust() {
    let f = typed_fory();
    let schema = schema();
    for c in cases(&f) {
        let decoded = schema.decode_as(c.type_name, &c.bytes).unwrap();
        // Through JSON, as a CLI would.
        let json_value = decoded.to_json(&JsonOptions::default());
        let from_json = schema
            .encode(c.type_name, &DynValue::from_json(&json_value))
            .unwrap_or_else(|e| panic!("{}: encode from JSON: {e}", c.name));
        assert_eq!(
            hex::encode(&from_json),
            hex::encode(&c.bytes),
            "{}: JSON",
            c.name
        );
        // And from the decoded DynValue directly.
        let direct = schema.encode(c.type_name, &decoded).unwrap();
        assert_eq!(direct, c.bytes, "{}: DynValue", c.name);
        (c.check)(&f, &from_json);
    }
}

#[test]
fn recorded_vectors_round_trip() {
    let schema = DynSchema::from_json(&load()["types"].to_string()).unwrap();
    let doc = load();
    for (section, enc) in [
        ("cases", StringEncoding::Utf8),
        ("pyfory_cases", StringEncoding::Pyfory),
    ] {
        let opts = EncodeOptions {
            string_encoding: enc,
            ..EncodeOptions::default()
        };
        for c in doc[section].as_array().unwrap() {
            let name = c["name"].as_str().unwrap();
            let ty = c["type"].as_str().unwrap();
            let bytes = hex::decode(c["hex"].as_str().unwrap()).unwrap();
            let decoded = schema
                .decode_as(ty, &bytes)
                .unwrap_or_else(|e| panic!("{section}/{name}: decode: {e}"));
            assert_eq!(
                decoded.to_json(&JsonOptions::default()),
                c["value"],
                "{section}/{name}: decoded value"
            );
            let encoded = schema
                .encode_with(ty, &DynValue::from_json(&c["value"]), &opts)
                .unwrap_or_else(|e| panic!("{section}/{name}: encode: {e}"));
            assert_eq!(
                hex::encode(encoded),
                c["hex"].as_str().unwrap(),
                "{section}/{name}: encoded bytes"
            );
        }
    }
    assert!(
        !doc["pyfory_cases"].as_array().unwrap().is_empty(),
        "pyfory_cases missing"
    );
}
