#![cfg(feature = "rpc")]
//! Contract identity and wire for the canonical temporal / decimal types and
//! for recursive payload types, published as
//! `conformance/vectors/contract-types.json` so the other bindings can check
//! they compute the same type hashes and decode the same payload bytes
//! (`tests/python/conformance/test_rust_golden_vectors.py`).
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --features rpc --test it contract_types::`

use std::path::PathBuf;

use aster::rpc::{
    contract_id, AsterType, BigInt, Date, Decimal, MethodDef, MethodPattern, PayloadRegistry,
    ScopeKind, ServiceContract, Timestamp, TypeDef, WireField,
};
use aster_transport_core::contract::{canonical_xlang_bytes_type_def, TypeKind};
use fory_derive::ForyStruct;
use serde_json::{json, Value};

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Temporal")]
struct Temporal {
    at: Timestamp,
    day: Date,
    amount: Decimal,
    maybe_at: Option<Timestamp>,
    history: Vec<Timestamp>,
}

/// Direct self-recursion (spec Appendix B.1) through a list.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/TreeNode")]
struct TreeNode {
    value: String,
    children: Vec<TreeNode>,
}

/// Direct self-recursion through `Option<Box<Self>>`. Contract identity only:
/// Fory 1.7.6's derived `reserved_space()` recurses through `Box` / `Option`
/// without a base case, so serializing a type whose cycle passes only through
/// `Box` / `Option` overflows the stack. A cycle with a `Vec` on it is fine.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Chain")]
struct Chain {
    value: i32,
    next: Option<Box<Chain>>,
}

/// Two-type mutual recursion (spec Appendix B.2).
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Author")]
struct Author {
    name: String,
    books: Vec<Book>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Book")]
struct Book {
    title: String,
    written_by: Option<Box<Author>>,
}

/// Three-type cycle (spec Appendix B.3); its closing edge is a list so Fory
/// can serialize it (see `Chain`).
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Alpha")]
struct Alpha {
    next: Option<Box<Beta>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Beta")]
struct Beta {
    next: Option<Box<Gamma>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Gamma")]
struct Gamma {
    next: Vec<Alpha>,
}

/// A non-recursive holder of a recursive type: an ordinary `REF` to it.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Forest")]
struct Forest {
    trees: Vec<TreeNode>,
}

fn field<'a>(td: &'a TypeDef, name: &str) -> &'a aster_transport_core::contract::FieldDef {
    td.fields.iter().find(|f| f.name == name).unwrap()
}

#[test]
fn temporal_and_decimal_map_to_canonical_primitives() {
    let td = Temporal::aster_type_def();
    for (name, prim, optional, container) in [
        ("at", "timestamp", false, 0u8),
        ("day", "date", false, 0),
        ("amount", "decimal", false, 0),
        ("maybe_at", "timestamp", true, 0),
        ("history", "timestamp", false, 1),
    ] {
        let f = field(&td, name);
        assert_eq!(f.type_kind, TypeKind::Primitive, "{name}");
        assert_eq!(f.type_primitive, prim, "{name}");
        assert_eq!(f.optional, optional, "{name}");
        assert_eq!(f.container as u8, container, "{name}");
    }
}

#[test]
fn self_recursion_is_a_self_ref() {
    let td = TreeNode::aster_type_def();
    let children = field(&td, "children");
    assert_eq!(children.type_kind, TypeKind::SelfRef);
    assert_eq!(children.self_ref_name, "types.v1.TreeNode");
    assert!(children.type_ref.is_empty());
    assert_eq!(children.container as u8, 1);
    assert!(!children.optional);

    // A box is transparent: `Option<Box<Self>>` describes like `Option<Self>`.
    let td = Chain::aster_type_def();
    let next = field(&td, "next");
    assert_eq!(next.type_kind, TypeKind::SelfRef);
    assert_eq!(next.self_ref_name, "types.v1.Chain");
    assert!(next.optional);
    assert_eq!(next.container as u8, 0);
}

#[test]
fn mutual_recursion_breaks_the_spanning_tree_back_edge() {
    // Author < Book: the tree edge is Author → Book, the back-edge Book → Author.
    let book = Book::aster_type_def();
    let written_by = field(&book, "written_by");
    assert_eq!(written_by.type_kind, TypeKind::SelfRef);
    assert_eq!(written_by.self_ref_name, "types.v1.Author");

    // A type's identity does not depend on which SCC member was asked for
    // first: Author's REF carries Book's own hash.
    let author = Author::aster_type_def();
    let books = field(&author, "books");
    assert_eq!(books.type_kind, TypeKind::Ref);
    assert_eq!(books.type_ref, Book::aster_type_hash_hex());
}

#[test]
fn three_cycle_breaks_only_the_closing_edge() {
    let alpha = Alpha::aster_type_def();
    let beta = Beta::aster_type_def();
    let gamma = Gamma::aster_type_def();
    assert_eq!(field(&alpha, "next").type_ref, Beta::aster_type_hash_hex());
    assert_eq!(field(&beta, "next").type_ref, Gamma::aster_type_hash_hex());
    let closing = field(&gamma, "next");
    assert_eq!(closing.type_kind, TypeKind::SelfRef);
    assert_eq!(closing.self_ref_name, "types.v1.Alpha");
}

#[test]
fn reference_to_a_recursive_type_is_a_plain_ref() {
    let td = Forest::aster_type_def();
    let trees = field(&td, "trees");
    assert_eq!(trees.type_kind, TypeKind::Ref);
    assert_eq!(trees.type_ref, TreeNode::aster_type_hash_hex());
}

#[test]
fn box_does_not_change_identity() {
    #[derive(ForyStruct, aster::AsterType, Default)]
    #[aster(wire = "types.v1/Holder")]
    struct Boxed {
        inner: Box<Temporal>,
        maybe: Option<Box<Temporal>>,
    }
    #[derive(ForyStruct, aster::AsterType, Default)]
    #[aster(wire = "types.v1/Holder")]
    struct Plain {
        inner: Temporal,
        maybe: Option<Temporal>,
    }
    assert_eq!(Boxed::aster_type_hash(), Plain::aster_type_hash());
}

// ── Wire: Rust round trip ────────────────────────────────────────────────────

fn fory_for<T: WireField>() -> fory_core::Fory {
    let mut reg = PayloadRegistry::new();
    T::register_payload(&mut reg);
    reg.into_fory()
}

fn sample_temporal() -> Temporal {
    Temporal {
        // 2024-03-01T12:34:56.789012Z — microsecond precision, the canonical
        // `timestamp` resolution (pyfory writes nanos = micros * 1000).
        at: Timestamp::new(1_709_296_496, 789_012_000).unwrap(),
        // 2024-03-01
        day: Date::from_epoch_days(19_783),
        // -12345.6789: one small-encoded value ...
        amount: Decimal::new(BigInt::from(-123_456_789), 4),
        maybe_at: None,
        history: vec![
            Timestamp::new(0, 0).unwrap(),
            Timestamp::new(-1, 999_999_000).unwrap(),
        ],
    }
}

fn sample_big_decimal() -> Temporal {
    Temporal {
        // ... and one past i64, the length-prefixed magnitude form.
        amount: Decimal::new(
            "123456789012345678901234567890".parse::<BigInt>().unwrap(),
            10,
        ),
        maybe_at: Some(Timestamp::new(1_700_000_000, 0).unwrap()),
        ..sample_temporal()
    }
}

fn sample_tree() -> TreeNode {
    TreeNode {
        value: "root".into(),
        children: vec![
            TreeNode {
                value: "a".into(),
                ..Default::default()
            },
            TreeNode {
                value: "b".into(),
                children: vec![TreeNode {
                    value: "b1".into(),
                    ..Default::default()
                }],
            },
        ],
    }
}

fn sample_author() -> Author {
    Author {
        name: "Ursula".into(),
        books: vec![Book {
            title: "The Dispossessed".into(),
            written_by: Some(Box::new(Author {
                name: "Ursula".into(),
                books: vec![],
            })),
        }],
    }
}

fn roundtrip<T>(value: &T) -> Vec<u8>
where
    T: WireField + fory_core::Serializer<Target = T> + PartialEq + std::fmt::Debug,
{
    let fory = fory_for::<T>();
    let bytes = fory.serialize(value).expect("serialize");
    let back: T = fory.deserialize(&bytes).expect("deserialize");
    assert_eq!(&back, value);
    bytes
}

#[test]
fn temporal_and_decimal_roundtrip() {
    roundtrip(&sample_temporal());
    roundtrip(&sample_big_decimal());
}

#[test]
fn recursive_values_roundtrip() {
    roundtrip(&sample_tree());
    roundtrip(&sample_author());
    roundtrip(&sample_cycle());
}

fn sample_cycle() -> Alpha {
    Alpha {
        next: Some(Box::new(Beta {
            next: Some(Box::new(Gamma {
                next: vec![Alpha::default()],
            })),
        })),
    }
}

#[test]
fn box_does_not_change_the_wire() {
    #[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
    #[aster(wire = "types.v1/Holder")]
    struct Boxed {
        inner: Box<Book>,
    }
    #[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
    #[aster(wire = "types.v1/Holder")]
    struct Plain {
        inner: Book,
    }
    let book = Book {
        title: "t".into(),
        written_by: None,
    };
    let boxed = roundtrip(&Boxed {
        inner: Box::new(book.clone()),
    });
    let plain = roundtrip(&Plain { inner: book });
    assert_eq!(boxed, plain);
}

// ── Cross-binding vector ─────────────────────────────────────────────────────

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors/contract-types.json")
}

fn type_entry<T: AsterType>() -> Value {
    let td = T::aster_type_def();
    let fields: Vec<Value> = td
        .fields
        .iter()
        .map(|f| {
            json!({
                "name": f.name,
                "type_kind": f.type_kind as u8,
                "type_primitive": f.type_primitive,
                "type_ref": f.type_ref,
                "self_ref_name": f.self_ref_name,
                "optional": f.optional,
                "container": f.container as u8,
            })
        })
        .collect();
    json!({
        "package": td.package,
        "name": td.name,
        "fields_in_declaration_order": fields,
        "expected_canonical_type_def_hex": hex::encode(canonical_xlang_bytes_type_def(&td)),
        "expected_type_hash": T::aster_type_hash_hex(),
    })
}

fn unary(name: &str, req: String, resp: String) -> MethodDef {
    MethodDef {
        name: name.into(),
        pattern: MethodPattern::Unary,
        request_type: req,
        response_type: resp,
        idempotent: false,
        default_timeout: 0.0,
        requires: None,
    }
}

fn service() -> ServiceContract {
    ServiceContract {
        name: "Types".into(),
        version: 1,
        methods: vec![
            unary(
                "echo_temporal",
                Temporal::aster_type_hash_hex(),
                Temporal::aster_type_hash_hex(),
            ),
            unary(
                "plant",
                TreeNode::aster_type_hash_hex(),
                Forest::aster_type_hash_hex(),
            ),
            unary(
                "author",
                Book::aster_type_hash_hex(),
                Author::aster_type_hash_hex(),
            ),
            unary(
                "cycle",
                Alpha::aster_type_hash_hex(),
                Gamma::aster_type_hash_hex(),
            ),
        ],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    }
}

fn actual() -> Value {
    json!({
        "types": {
            "types.v1.Temporal": type_entry::<Temporal>(),
            "types.v1.TreeNode": type_entry::<TreeNode>(),
            "types.v1.Author": type_entry::<Author>(),
            "types.v1.Book": type_entry::<Book>(),
            "types.v1.Alpha": type_entry::<Alpha>(),
            "types.v1.Beta": type_entry::<Beta>(),
            "types.v1.Gamma": type_entry::<Gamma>(),
            "types.v1.Forest": type_entry::<Forest>(),
            "types.v1.Chain": type_entry::<Chain>(),
        },
        "service": "Types v1, xlang, SHARED; unary methods echo_temporal(Temporal)->Temporal, plant(TreeNode)->Forest, author(Book)->Author, cycle(Alpha)->Gamma",
        "expected_contract_id": contract_id(&service()),
        "payloads": {
            "temporal_small_decimal": {
                "type": "types.v1.Temporal",
                "hex": hex::encode(roundtrip(&sample_temporal())),
                "value": {
                    "at": "2024-03-01T12:34:56.789012Z",
                    "day": "2024-03-01",
                    "amount": "-12345.6789",
                    "maybe_at": null,
                    "history": ["1970-01-01T00:00:00.000000Z", "1969-12-31T23:59:59.999999Z"],
                },
            },
            "temporal_big_decimal": {
                "type": "types.v1.Temporal",
                "hex": hex::encode(roundtrip(&sample_big_decimal())),
                "value": {
                    "at": "2024-03-01T12:34:56.789012Z",
                    "day": "2024-03-01",
                    "amount": "12345678901234567890.1234567890",
                    "maybe_at": "2023-11-14T22:13:20.000000Z",
                    "history": ["1970-01-01T00:00:00.000000Z", "1969-12-31T23:59:59.999999Z"],
                },
            },
            "tree": {
                "type": "types.v1.TreeNode",
                "hex": hex::encode(roundtrip(&sample_tree())),
                "value": {
                    "value": "root",
                    "children": [
                        {"value": "a", "children": []},
                        {"value": "b", "children": [{"value": "b1", "children": []}]},
                    ],
                },
            },
            "cycle": {
                "type": "types.v1.Alpha",
                "hex": hex::encode(roundtrip(&sample_cycle())),
                "value": {"next": {"next": {"next": [{"next": null}]}}},
            },
            "author": {
                "type": "types.v1.Author",
                "hex": hex::encode(roundtrip(&sample_author())),
                "value": {
                    "name": "Ursula",
                    "books": [{"title": "The Dispossessed", "written_by": {"name": "Ursula", "books": []}}],
                },
            },
        },
    })
}

#[test]
fn contract_types_vector() {
    let got = actual();
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        let doc = json!({
            "description": "Contract-identity and wire vectors for the canonical timestamp / date / decimal types and for recursive types (Aster-ContractIdentity.md, 'Language-Native Default Mapping' Rust rows and §11.3.4 Step 3 / Appendix B). Produced by the Rust #[derive(AsterType)].",
            "rules": {
                "timestamp": "Fory xlang TIMESTAMP (type id 38): int64 seconds + uint32 nanos since the Unix epoch, UTC",
                "date": "Fory xlang DATE (type id 39): varint64 days since 1970-01-01",
                "decimal": "Fory xlang DECIMAL (type id 40): varint32 scale, then the unscaled value (zig-zag varint64 << 1, or a flagged little-endian magnitude)",
                "self_ref": "an edge outside an SCC's spanning tree (DFS from the codepoint-smallest member, successors in codepoint order) is type_kind SELF_REF (2) with self_ref_name = the target's package.name; tree edges are REF (1) to the target's hash",
                "box": "Rust Box<T> is transparent in both the contract and the wire"
            },
            "rust_source": "aster/tests/it/contract_types.rs",
            "vector": got,
        });
        std::fs::write(path(), serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(doc["vector"], got, "contract-types vector drifted");
}
