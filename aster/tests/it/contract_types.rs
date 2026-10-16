#![cfg(feature = "rpc")]
//! Contract identity and wire for the canonical temporal / decimal types, the
//! `uuid` / `ulid` identifiers and `decimal(p, s)`, and for recursive payload
//! types, published as
//! `conformance/vectors/contract-types.json` so the other bindings can check
//! they compute the same type hashes and decode the same payload bytes
//! (`tests/python/conformance/test_rust_golden_vectors.py`).
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --features rpc --test it contract_types::`

use std::collections::BTreeMap;
use std::path::PathBuf;

use aster::rpc::{
    contract_id, AsterType, BigInt, Date, Decimal, FixedDecimal, MethodDef, MethodPattern,
    PayloadRegistry, ScopeKind, ServiceContract, Timestamp, TypeDef, Ulid, Uuid, WireField,
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

/// A three-type cycle whose spanning tree depends on ordering names by
/// codepoint (CT-3): `Ａ` (U+FF21) sorts before `𝔸` (U+1D538) by codepoint
/// but after it by UTF-16 code unit (a surrogate pair starts at U+D835), so a
/// binding that orders successors by UTF-16 visits `𝔸` first from `Ord` and
/// breaks different edges. Contract identity only (a cycle through `Option`).
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Ord")]
struct CpOrd {
    wide: Option<Box<CpWide>>,
    math: Option<Box<CpMath>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/\u{FF21}")]
struct CpWide {
    back: Option<Box<CpOrd>>,
    math: Option<Box<CpMath>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/\u{1D538}")]
struct CpMath {
    wide: Option<Box<CpWide>>,
}

/// The spec's B.4 diamond: A -> B, A -> C, B -> C, C -> A. The DFS from A
/// reaches C through B, so C is hashed before A, yet A -> C is still a
/// back-edge: a binding must check the back-edge before looking up the
/// (already known) hash. Contract identity only.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/DiamondA")]
struct DiamondA {
    b: Option<Box<DiamondB>>,
    c: Option<Box<DiamondC>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/DiamondB")]
struct DiamondB {
    c: Option<Box<DiamondC>>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/DiamondC")]
struct DiamondC {
    a: Vec<DiamondA>,
}

/// A non-recursive holder of a recursive type: an ordinary `REF` to it.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Forest")]
struct Forest {
    trees: Vec<TreeNode>,
}

/// The 16-byte identifiers: `uuid` and `ulid`, as scalars, nullable, list
/// elements and map keys.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Identified")]
struct Identified {
    #[aster(uuid)]
    id: [u8; 16],
    #[aster(ulid)]
    trace: [u8; 16],
    #[aster(uuid)]
    parent: Option<[u8; 16]>,
    #[aster(ulid)]
    history: Vec<[u8; 16]>,
    #[aster(uuid)]
    names: BTreeMap<[u8; 16], String>,
}

/// `decimal(p, s)` beside the unconstrained `decimal`.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Priced")]
struct Priced {
    price: FixedDecimal<18, 4>,
    rate: FixedDecimal<5, 0>,
    maybe_price: Option<FixedDecimal<18, 4>>,
    amount: Decimal,
}

/// `Priced` with unconstrained decimals, standing in for a peer that sends
/// values outside `decimal(18,4)` / `decimal(5,0)` (same wire name and fields).
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Priced")]
struct LoosePriced {
    price: Decimal,
    rate: Decimal,
    maybe_price: Option<Decimal>,
    amount: Decimal,
}

/// `Identified` with plain byte strings: the wire `uuid` / `ulid` share with
/// `binary`, and a peer that sends the wrong length.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "types.v1/Identified")]
struct LooseIdentified {
    #[fory(bytes)]
    id: Vec<u8>,
    #[fory(bytes)]
    trace: Vec<u8>,
    #[fory(bytes)]
    parent: Option<Vec<u8>>,
    #[fory(list(element(bytes)))]
    history: Vec<Vec<u8>>,
    #[fory(map(key(bytes)))]
    names: BTreeMap<Vec<u8>, String>,
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
fn identifiers_and_fixed_decimals_map_to_canonical_primitives() {
    let td = Identified::aster_type_def();
    for (name, prim, optional, container) in [
        ("id", "uuid", false, 0u8),
        ("trace", "ulid", false, 0),
        ("parent", "uuid", true, 0),
        ("history", "ulid", false, 1),
        ("names", "string", false, 3),
    ] {
        let f = field(&td, name);
        assert_eq!(f.type_kind, TypeKind::Primitive, "{name}");
        assert_eq!(f.type_primitive, prim, "{name}");
        assert_eq!(f.optional, optional, "{name}");
        assert_eq!(f.container as u8, container, "{name}");
    }
    assert_eq!(field(&td, "names").container_key_primitive, "uuid");

    let td = Priced::aster_type_def();
    assert_eq!(field(&td, "price").type_primitive, "decimal(18,4)");
    assert_eq!(field(&td, "rate").type_primitive, "decimal(5,0)");
    assert_eq!(field(&td, "maybe_price").type_primitive, "decimal(18,4)");
    assert_eq!(field(&td, "amount").type_primitive, "decimal");
    // (p, s) is part of identity: the loose mirror hashes differently.
    assert_ne!(Priced::aster_type_hash(), LoosePriced::aster_type_hash());
}

#[test]
fn uuid_and_ulid_are_distinct_contract_types() {
    #[derive(ForyStruct, aster::AsterType, Default)]
    #[aster(wire = "types.v1/OneId")]
    struct ByUuid {
        #[aster(uuid)]
        id: [u8; 16],
    }
    #[derive(ForyStruct, aster::AsterType, Default)]
    #[aster(wire = "types.v1/OneId")]
    struct ByUlid {
        #[aster(ulid)]
        id: [u8; 16],
    }
    #[derive(ForyStruct, aster::AsterType, Default)]
    #[aster(wire = "types.v1/OneId")]
    struct ByBytes {
        id: [u8; 16],
    }
    assert_ne!(ByUuid::aster_type_hash(), ByUlid::aster_type_hash());
    assert_ne!(ByUuid::aster_type_hash(), ByBytes::aster_type_hash());
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
fn spanning_tree_orders_names_by_codepoint_not_utf16() {
    // Codepoint order from Ord: Ord -> Ａ -> 𝔸 is the tree; Ａ -> Ord,
    // 𝔸 -> Ａ and Ord -> 𝔸 are the back-edges.
    let ord = CpOrd::aster_type_def();
    assert_eq!(field(&ord, "wide").type_ref, CpWide::aster_type_hash_hex());
    assert_eq!(field(&ord, "math").type_kind, TypeKind::SelfRef);
    assert_eq!(field(&ord, "math").self_ref_name, "types.v1.\u{1D538}");
    let wide = CpWide::aster_type_def();
    assert_eq!(field(&wide, "back").type_kind, TypeKind::SelfRef);
    assert_eq!(field(&wide, "math").type_ref, CpMath::aster_type_hash_hex());
    let math = CpMath::aster_type_def();
    assert_eq!(field(&math, "wide").type_kind, TypeKind::SelfRef);
    assert_eq!(field(&math, "wide").self_ref_name, "types.v1.\u{FF21}");
}

#[test]
fn diamond_back_edge_to_an_already_hashed_type_is_a_self_ref() {
    let a = DiamondA::aster_type_def();
    assert_eq!(field(&a, "b").type_ref, DiamondB::aster_type_hash_hex());
    let c = field(&a, "c");
    assert_eq!(c.type_kind, TypeKind::SelfRef);
    assert_eq!(c.self_ref_name, "types.v1.DiamondC");
    let b = DiamondB::aster_type_def();
    assert_eq!(field(&b, "c").type_ref, DiamondC::aster_type_hash_hex());
    let c = DiamondC::aster_type_def();
    assert_eq!(field(&c, "a").type_kind, TypeKind::SelfRef);
    assert_eq!(field(&c, "a").self_ref_name, "types.v1.DiamondA");
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

const SAMPLE_UUID: &str = "0190f2a8-1b2c-7d3e-8f40-123456789abc";
const SAMPLE_ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn sample_identified() -> Identified {
    let id: [u8; 16] = SAMPLE_UUID.parse::<Uuid>().unwrap().into();
    let trace: [u8; 16] = SAMPLE_ULID.parse::<Ulid>().unwrap().into();
    Identified {
        id,
        trace,
        parent: None,
        history: vec![trace, Ulid::from_u128(0).into()],
        names: BTreeMap::from([(id, "primary".to_string())]),
    }
}

fn loose(id: &Identified) -> LooseIdentified {
    LooseIdentified {
        id: id.id.to_vec(),
        trace: id.trace.to_vec(),
        parent: id.parent.map(|p| p.to_vec()),
        history: id.history.iter().map(|u| u.to_vec()).collect(),
        names: id
            .names
            .iter()
            .map(|(k, v)| (k.to_vec(), v.clone()))
            .collect(),
    }
}

fn sample_priced() -> Priced {
    Priced {
        price: "-12345.67".parse().unwrap(),
        rate: "99999".parse().unwrap(),
        maybe_price: Some("0.0001".parse().unwrap()),
        amount: Decimal::new(BigInt::from(15), 1),
    }
}

/// A `Decimal` at exactly the scale written: `"1.50"` is unscaled 150, scale 2.
fn dec(s: &str) -> Decimal {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    Decimal::new(format!("{int}{frac}").parse().unwrap(), frac.len() as i32)
}

fn loose_priced(price: &str, rate: &str) -> LoosePriced {
    LoosePriced {
        price: dec(price),
        rate: dec(rate),
        maybe_price: None,
        amount: dec("0"),
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
fn identifiers_roundtrip_as_16_byte_binary() {
    let id = sample_identified();
    let bytes = roundtrip(&id);
    // Byte-for-byte the wire of `binary` fields holding the same 16 bytes.
    let fory = fory_for::<LooseIdentified>();
    assert_eq!(fory.serialize(&loose(&id)).unwrap(), bytes);
    roundtrip(&Identified {
        parent: Some(Uuid::from_u128(1).into()),
        ..id
    });
}

#[test]
fn wrong_length_identifier_is_refused() {
    let fory = fory_for::<Identified>();
    let send = |l: &LooseIdentified| fory_for::<LooseIdentified>().serialize(l).unwrap();
    for bad in [15usize, 17, 0] {
        let mut l = loose(&sample_identified());
        l.id = vec![7; bad];
        let err = fory.deserialize::<Identified>(&send(&l)).unwrap_err();
        assert!(
            err.to_string().contains("expected 16 bytes"),
            "{bad}: {err}"
        );
    }
    let mut l = loose(&sample_identified());
    l.history[1] = vec![0; 32];
    assert!(fory.deserialize::<Identified>(&send(&l)).is_err());
}

#[test]
fn fixed_decimal_roundtrip_writes_the_declared_scale() {
    let bytes = roundtrip(&sample_priced());
    let back: LoosePriced = fory_for::<LoosePriced>().deserialize(&bytes).unwrap();
    assert_eq!(back.price, Decimal::new(BigInt::from(-123_456_700), 4));
    assert_eq!(back.rate, Decimal::new(BigInt::from(99_999), 0));
}

#[test]
fn fixed_decimal_decode_applies_the_constraint() {
    let fory = fory_for::<Priced>();
    let send = |l: &LoosePriced| fory_for::<LoosePriced>().serialize(l).unwrap();
    // Zero padding is exact: 1.5 arrives at scale 1 and decodes at scale 4.
    let ok: Priced = fory.deserialize(&send(&loose_priced("1.5", "7"))).unwrap();
    assert_eq!(
        ok.price.as_decimal(),
        &Decimal::new(BigInt::from(15_000), 4)
    );
    // Trailing zeros past the scale drop exactly.
    let ok: Priced = fory
        .deserialize(&send(&loose_priced("1.500000", "7.0")))
        .unwrap();
    assert_eq!(ok.price.to_string(), "1.5000");
    assert_eq!(ok.rate.to_string(), "7");
    // Needs rounding.
    let err = fory
        .deserialize::<Priced>(&send(&loose_priced("1.23456", "7")))
        .unwrap_err();
    assert!(err.to_string().contains("rounding"), "{err}");
    // More than 18 digits at scale 4.
    let err = fory
        .deserialize::<Priced>(&send(&loose_priced("123456789012345", "7")))
        .unwrap_err();
    assert!(err.to_string().contains("more than 18 digits"), "{err}");
    // More than 5 digits at scale 0.
    assert!(fory
        .deserialize::<Priced>(&send(&loose_priced("1", "123456")))
        .is_err());
}

#[test]
fn fixed_decimal_encode_refuses_what_does_not_fit() {
    assert!("1.23456".parse::<FixedDecimal<18, 4>>().is_err());
    assert!("123456789012345".parse::<FixedDecimal<18, 4>>().is_err());
    assert!(FixedDecimal::<5, 0>::new(dec("0.5")).is_err());
    assert_eq!(
        FixedDecimal::<18, 4>::new(dec("2")).unwrap().as_decimal(),
        &Decimal::new(BigInt::from(20_000), 4)
    );
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

// ── Field names (ffi_spec/Aster-FieldNaming.md) ──────────────────────────────

/// A contract defined in Rust: its field names are the Rust (snake_case) ones.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "names.v1/Order")]
struct RsOrder {
    order_id: String,
    line_items: Vec<RsLineItem>,
    total_cents: i32,
    note: Option<String>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "names.v1/LineItem")]
struct RsLineItem {
    sku_code: String,
    unit_count: i32,
}

/// The same wire types as a contract defined in Java (`orderId`, ...): Rust
/// reproduces it with name overrides. Same Fory bytes, other contract.
#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "names.v1/Order")]
struct JvOrder {
    #[aster(name = "orderId")]
    order_id: String,
    #[aster(name = "lineItems")]
    line_items: Vec<JvLineItem>,
    #[aster(name = "totalCents")]
    total_cents: i32,
    note: Option<String>,
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "names.v1/LineItem")]
struct JvLineItem {
    #[aster(name = "skuCode")]
    sku_code: String,
    #[aster(name = "unitCount")]
    unit_count: i32,
}

fn sample_order() -> RsOrder {
    RsOrder {
        order_id: "o-1".into(),
        line_items: vec![
            RsLineItem {
                sku_code: "sku-a".into(),
                unit_count: 2,
            },
            RsLineItem {
                sku_code: "sku-b".into(),
                unit_count: 1,
            },
        ],
        total_cents: 1250,
        note: None,
    }
}

fn sample_jv_order() -> JvOrder {
    let o = sample_order();
    JvOrder {
        order_id: o.order_id,
        line_items: o
            .line_items
            .into_iter()
            .map(|l| JvLineItem {
                sku_code: l.sku_code,
                unit_count: l.unit_count,
            })
            .collect(),
        total_cents: o.total_cents,
        note: o.note,
    }
}

/// Each Fory implementation's snake_case form of awkward field names:
/// (name, plain, [rust, go, java, csharp, javascript, python]).
const SNAKE_CASE: &[(&str, bool, [&str; 6])] = &[
    (
        "value",
        true,
        ["value", "value", "value", "value", "value", "value"],
    ),
    (
        "writtenBy",
        true,
        [
            "written_by",
            "written_by",
            "written_by",
            "written_by",
            "written_by",
            "written_by",
        ],
    ),
    (
        "WrittenBy",
        true,
        [
            "written_by",
            "written_by",
            "_written_by",
            "written_by",
            "written_by",
            "written_by",
        ],
    ),
    (
        "written_by",
        true,
        [
            "written_by",
            "written_by",
            "written_by",
            "written_by",
            "written_by",
            "written_by",
        ],
    ),
    (
        "orderId",
        true,
        [
            "order_id", "order_id", "order_id", "order_id", "order_id", "order_id",
        ],
    ),
    (
        "OrderId",
        true,
        [
            "order_id",
            "order_id",
            "_order_id",
            "order_id",
            "order_id",
            "order_id",
        ],
    ),
    ("ID", false, ["id", "i_d", "_i_d", "id", "id", "id"]),
    ("Id", true, ["id", "id", "_id", "id", "id", "id"]),
    (
        "userID",
        false,
        [
            "user_id", "user_i_d", "user_i_d", "user_id", "user_id", "user_id",
        ],
    ),
    (
        "userId",
        true,
        [
            "user_id", "user_id", "user_id", "user_id", "user_id", "user_id",
        ],
    ),
    (
        "user_id",
        true,
        [
            "user_id", "user_id", "user_id", "user_id", "user_id", "user_id",
        ],
    ),
    (
        "userId2",
        true,
        [
            "user_id2", "user_id2", "user_id2", "user_id2", "user_id2", "user_id2",
        ],
    ),
    (
        "userID2",
        false,
        [
            "user_i_d2",
            "user_i_d2",
            "user_i_d2",
            "user_i_d2",
            "user_i_d2",
            "user_id2",
        ],
    ),
    (
        "HTTPServer",
        false,
        [
            "http_server",
            "h_t_t_p_server",
            "_h_t_t_p_server",
            "http_server",
            "http_server",
            "http_server",
        ],
    ),
    (
        "HttpServer",
        true,
        [
            "http_server",
            "http_server",
            "_http_server",
            "http_server",
            "http_server",
            "http_server",
        ],
    ),
    (
        "httpServer",
        true,
        [
            "http_server",
            "http_server",
            "http_server",
            "http_server",
            "http_server",
            "http_server",
        ],
    ),
    (
        "x509Cert",
        true,
        [
            "x509_cert",
            "x509_cert",
            "x509_cert",
            "x509_cert",
            "x509_cert",
            "x509_cert",
        ],
    ),
    (
        "X509Cert",
        true,
        [
            "x509_cert",
            "x509_cert",
            "_x509_cert",
            "x509_cert",
            "x509_cert",
            "x509_cert",
        ],
    ),
    (
        "ipv6Addr",
        true,
        [
            "ipv6_addr",
            "ipv6_addr",
            "ipv6_addr",
            "ipv6_addr",
            "ipv6_addr",
            "ipv6_addr",
        ],
    ),
    (
        "IPv6Addr",
        false,
        [
            "i_pv6_addr",
            "i_pv6_addr",
            "_i_pv6_addr",
            "i_pv6_addr",
            "i_pv6_addr",
            "i_pv6_addr",
        ],
    ),
    (
        "aBC1",
        false,
        ["a_b_c1", "a_b_c1", "a_b_c1", "a_b_c1", "a_b_c1", "a_bc1"],
    ),
    (
        "a_B",
        false,
        ["a__b", "a__b", "a__b", "a__b", "a__b", "a_b"],
    ),
    (
        "a__b",
        true,
        ["a__b", "a__b", "a__b", "a__b", "a__b", "a__b"],
    ),
    (
        "type_",
        false,
        ["type_", "type_", "type_", "type_", "type_", "type"],
    ),
    (
        "_private",
        true,
        [
            "_private", "_private", "_private", "_private", "_private", "_private",
        ],
    ),
    (
        "maxÉcart",
        false,
        [
            "maxÉcart",
            "maxÉcart",
            "maxÉcart",
            "max_écart",
            "maxÉcart",
            "max_écart",
        ],
    ),
    (
        "名Value",
        false,
        [
            "名_value",
            "名_value",
            "名_value",
            "名_value",
            "名_value",
            "名value",
        ],
    ),
    (
        "getHTTPResponse",
        false,
        [
            "get_http_response",
            "get_h_t_t_p_response",
            "get_h_t_t_p_response",
            "get_http_response",
            "get_http_response",
            "get_http_response",
        ],
    ),
];

const SNAKE_IMPLS: [&str; 6] = ["rust", "go", "java", "csharp", "javascript", "python"];

#[test]
fn override_names_the_field_and_keeps_the_wire() {
    let td = JvOrder::aster_type_def();
    let names: Vec<&str> = td.fields.iter().map(|f| f.name.as_str()).collect();
    for n in ["orderId", "lineItems", "totalCents", "note"] {
        assert!(names.contains(&n), "{names:?}");
    }
    assert_ne!(JvOrder::aster_type_hash(), RsOrder::aster_type_hash());
    // Fory matches by the snake_case form: the bytes are the same.
    assert_eq!(roundtrip(&sample_order()), roundtrip(&sample_jv_order()));
}

#[test]
fn rust_fory_snake_case_is_the_vector_reference() {
    for (name, plain, forms) in SNAKE_CASE {
        assert_eq!(fory_core::util::to_snake_case(name), forms[0], "{name}");
        // A plain name: every Fory implementation agrees, except Fory Java on
        // a leading capital (Java refuses such a member locally).
        if *plain {
            for (k, form) in forms.iter().enumerate() {
                if SNAKE_IMPLS[k] == "java" && name.starts_with(|c: char| c.is_ascii_uppercase()) {
                    continue;
                }
                assert_eq!(*form, forms[0], "{name}: {}", SNAKE_IMPLS[k]);
            }
        }
    }
}

fn orders_service(req: String) -> ServiceContract {
    ServiceContract {
        name: "Orders".into(),
        version: 1,
        methods: vec![unary("place", req.clone(), req)],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    }
}

fn field_names() -> Value {
    let snake: serde_json::Map<String, Value> = SNAKE_CASE
        .iter()
        .map(|(name, plain, forms)| {
            let mut row = serde_json::Map::new();
            row.insert("plain".into(), json!(plain));
            for (k, form) in forms.iter().enumerate() {
                row.insert(SNAKE_IMPLS[k].into(), json!(form));
            }
            (name.to_string(), Value::Object(row))
        })
        .collect();
    let field =
        |member_snake: &str, name: &str| json!({"member_snake": member_snake, "name": name});
    let mut refused = vec![
        json!({"case": "collision", "error": "collide",
               "fields": [field("user_id", "userId"), field("user_id", "user_id")]}),
        json!({"case": "override_mismatch", "error": "does not match",
               "fields": [field("author", "writtenBy")]}),
    ];
    for (name, plain, forms) in SNAKE_CASE {
        if !plain {
            refused.push(
                json!({"case": "not_plain", "error": "not a plain field name",
                                "fields": [field(forms[0], name)]}),
            );
        }
    }
    let accepted = json!([
        {"fields": [field("written_by", "writtenBy")]},
        {"fields": [field("written_by", "WrittenBy")]},
        {"fields": [field("written_by", "written_by")]},
        {"fields": [field("id", "Id")]},
        {"fields": [field("user_id2", "userId2")]},
        {"fields": [field("x509_cert", "x509Cert")]},
        {"fields": [field("http_server", "HttpServer")]},
        {"fields": [field("_private", "_private")]},
        {"fields": [field("order_id", "orderId"), field("line_items", "lineItems"),
                    field("total_cents", "totalCents"), field("note", "note")]},
    ]);
    let order_hex = hex::encode(roundtrip(&sample_order()));
    json!({
        "rules": {
            "name": "a field's contract name is the declared name in the language that defines the contract, verbatim; a binding consuming it elsewhere declares an explicit override. No binding converts case",
            "snake_case": "the reference snake_case form is Rust fory-core / Fory JS to_snake_case; snake_case lists each Fory implementation's form of awkward names",
            "plain": "a name is plain when every ASCII uppercase letter starts it or follows an ASCII lowercase letter or a digit, no non-ASCII letter is uppercase, and it does not end in _. For a plain name every Fory implementation writes the reference form (Fory Java prefixes _ to a leading capital, so Java refuses a member starting with one)",
            "refused": "contract definition refuses a name that is not plain, a field whose member's Fory snake_case form (member_snake) differs from the reference form of its name, and two fields with the same reference form; error is a phrase the message contains",
        },
        "snake_case": snake,
        "refused": refused,
        "accepted": accepted,
        "rust_defined": {
            "types": {
                "names.v1.Order": type_entry::<RsOrder>(),
                "names.v1.LineItem": type_entry::<RsLineItem>(),
            },
            "service": "Orders v1, xlang, SHARED; unary method place(Order)->Order",
            "expected_contract_id": contract_id(&orders_service(RsOrder::aster_type_hash_hex())),
            "payload": {
                "type": "names.v1.Order",
                "hex": order_hex,
                "value": {
                    "order_id": "o-1",
                    "line_items": [{"sku_code": "sku-a", "unit_count": 2},
                                   {"sku_code": "sku-b", "unit_count": 1}],
                    "total_cents": 1250,
                    "note": null,
                },
            },
        },
        "java_defined": {
            "types": {
                "names.v1.Order": type_entry::<JvOrder>(),
                "names.v1.LineItem": type_entry::<JvLineItem>(),
            },
            "service": "Orders v1, xlang, SHARED; unary method place(Order)->Order",
            "expected_contract_id": contract_id(&orders_service(JvOrder::aster_type_hash_hex())),
            "payload": {
                "type": "names.v1.Order",
                "hex": hex::encode(roundtrip(&sample_jv_order())),
                "value": {
                    "orderId": "o-1",
                    "lineItems": [{"skuCode": "sku-a", "unitCount": 2},
                                  {"skuCode": "sku-b", "unitCount": 1}],
                    "totalCents": 1250,
                    "note": null,
                },
            },
        },
    })
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
            "types.v1.Identified": type_entry::<Identified>(),
            "types.v1.Priced": type_entry::<Priced>(),
            "types.v1.Ord": type_entry::<CpOrd>(),
            "types.v1.\u{FF21}": type_entry::<CpWide>(),
            "types.v1.\u{1D538}": type_entry::<CpMath>(),
            "types.v1.DiamondA": type_entry::<DiamondA>(),
            "types.v1.DiamondB": type_entry::<DiamondB>(),
            "types.v1.DiamondC": type_entry::<DiamondC>(),
        },
        "service": "Types v1, xlang, SHARED; unary methods echo_temporal(Temporal)->Temporal, plant(TreeNode)->Forest, author(Book)->Author, cycle(Alpha)->Gamma",
        "expected_contract_id": contract_id(&service()),
        "ids_service": "Ids v1, xlang, SHARED; unary method price(Identified)->Priced",
        "expected_ids_contract_id": contract_id(&ids_service()),
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
            "identified": {
                "type": "types.v1.Identified",
                "hex": hex::encode(roundtrip(&sample_identified())),
                "value": {
                    "id": SAMPLE_UUID,
                    "trace": SAMPLE_ULID,
                    "parent": null,
                    "history": [SAMPLE_ULID, "00000000000000000000000000"],
                    "names": {SAMPLE_UUID: "primary"},
                },
            },
            "priced": {
                "type": "types.v1.Priced",
                "hex": hex::encode(roundtrip(&sample_priced())),
                "value": {
                    "price": "-12345.6700",
                    "rate": "99999",
                    "maybe_price": "0.0001",
                    "amount": "1.5",
                },
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
        "decode_cases": decode_cases(),
        "field_names": field_names(),
    })
}

/// Payloads a peer may send that a `Priced` / `Identified` decoder must accept
/// (with the value given) or refuse.
fn decode_cases() -> Value {
    let priced = |price: &str, rate: &str| {
        let l = loose_priced(price, rate);
        hex::encode(fory_for::<LoosePriced>().serialize(&l).unwrap())
    };
    let ids =
        |l: &LooseIdentified| hex::encode(fory_for::<LooseIdentified>().serialize(l).unwrap());
    let mut short = loose(&sample_identified());
    short.id = vec![0xab; 15];
    let mut long = loose(&sample_identified());
    long.history[0] = vec![0xab; 17];
    json!({
        "priced_zero_padding": {
            "type": "types.v1.Priced",
            "hex": priced("1.5", "7.0"),
            "sent": "price 1.5 (scale 1), rate 7.0 (scale 1)",
            "refused": false,
            "value": {"price": "1.5000", "rate": "7", "maybe_price": null, "amount": "0"},
        },
        "priced_needs_rounding": {
            "type": "types.v1.Priced",
            "hex": priced("1.23456", "7"),
            "sent": "price 1.23456 into decimal(18,4)",
            "refused": true,
        },
        "priced_too_many_digits": {
            "type": "types.v1.Priced",
            "hex": priced("123456789012345", "7"),
            "sent": "price 123456789012345 (19 digits at scale 4) into decimal(18,4)",
            "refused": true,
        },
        "priced_rate_too_many_digits": {
            "type": "types.v1.Priced",
            "hex": priced("1", "123456"),
            "sent": "rate 123456 into decimal(5,0)",
            "refused": true,
        },
        "uuid_15_bytes": {
            "type": "types.v1.Identified",
            "hex": ids(&short),
            "sent": "id as 15 bytes",
            "refused": true,
        },
        "ulid_17_bytes": {
            "type": "types.v1.Identified",
            "hex": ids(&long),
            "sent": "history[0] as 17 bytes",
            "refused": true,
        },
    })
}

fn ids_service() -> ServiceContract {
    ServiceContract {
        name: "Ids".into(),
        version: 1,
        methods: vec![unary(
            "price",
            Identified::aster_type_hash_hex(),
            Priced::aster_type_hash_hex(),
        )],
        serialization_modes: vec!["xlang".into()],
        scoped: ScopeKind::Shared,
        requires: None,
        producer_language: String::new(),
    }
}

#[test]
fn decode_cases_match_the_rust_decoder() {
    let cases = decode_cases();
    for (name, case) in cases.as_object().unwrap() {
        let bytes = hex::decode(case["hex"].as_str().unwrap()).unwrap();
        let refused = case["refused"].as_bool().unwrap();
        let ok = match case["type"].as_str().unwrap() {
            "types.v1.Priced" => fory_for::<Priced>().deserialize::<Priced>(&bytes).is_ok(),
            _ => fory_for::<Identified>()
                .deserialize::<Identified>(&bytes)
                .is_ok(),
        };
        assert_eq!(ok, !refused, "{name}");
    }
}

#[test]
fn contract_types_vector() {
    let got = actual();
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        let doc = json!({
            "description": "Contract-identity and wire vectors for the canonical timestamp / date / decimal / decimal(p,s) / uuid / ulid types and for recursive types (Aster-ContractIdentity.md, 'Language-Native Default Mapping' Rust rows and §11.3.4 Step 3 / Appendix B). Produced by the Rust #[derive(AsterType)].",
            "rules": {
                "timestamp": "Fory xlang TIMESTAMP (type id 38): int64 seconds + uint32 nanos since the Unix epoch, UTC",
                "date": "Fory xlang DATE (type id 39): varint64 days since 1970-01-01",
                "decimal": "Fory xlang DECIMAL (type id 40): varint32 scale, then the unscaled value (zig-zag varint64 << 1, or a flagged little-endian magnitude)",
                "self_ref": "an edge outside an SCC's spanning tree (DFS from the codepoint-smallest member, successors in codepoint order: Unicode scalar values after NFC, not UTF-16 code units, see types.v1.Ord) is type_kind SELF_REF (2) with self_ref_name = the target's package.name; tree edges are REF (1) to the target's hash",
                "box": "Rust Box<T> is transparent in both the contract and the wire",
                "uuid": "contract type uuid: Fory xlang BINARY (type id 41) of exactly 16 bytes (varuint32 length 16, then the bytes); a decoder refuses any other length. String form 8-4-4-4-12 lowercase hex",
                "ulid": "contract type ulid (an identity distinct from uuid): the same 16-byte BINARY wire; string form 26 characters of Crockford base32",
                "decimal(p,s)": "type_primitive is the ASCII string decimal(p,s) with no spaces, e.g. decimal(18,4). Wire: Fory DECIMAL with scale exactly s; the encoder zero-pads to scale s and refuses a value that needs rounding or has more than p digits; the decoder applies the same rule. decode_cases lists payloads a decoder must accept (with the value) or refuse"
            },
            "rust_source": "aster/tests/it/contract_types.rs",
            "vector": got,
        });
        std::fs::write(path(), serde_json::to_string_pretty(&doc).unwrap() + "\n").unwrap();
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(doc["vector"], got, "contract-types vector drifted");
}
