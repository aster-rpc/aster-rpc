//! Dynamic (schema-from-contract) Fory codec.
//!
//! Encodes and decodes Aster RPC payloads without generated types: the shape
//! comes from the contract's `TypeDef`s (the `types/{hash}.bin` blobs every
//! published contract carries), the values are [`DynValue`]s or JSON. The
//! bytes are Fory **1.7.6** xlang with Aster's fixed options
//! (`xlang=true, compatible=true, ref=false`, [`crate::xlang::new_fory`]) and
//! are byte-identical to what a typed Rust runtime (`#[derive(ForyStruct)]` +
//! `register_by_name("package.Name")`) writes for the same value — pinned by
//! `conformance/vectors/dynamic-payloads.json`.
//!
//! This is the one implementation every binding's proxy client / CLI shell
//! uses (plan D5): Python and TypeScript through PyO3/NAPI, Go/Java/.NET
//! through the C ABI. The entry points are deliberately bytes/JSON in, bytes/
//! JSON out:
//!
//! - [`DynSchema`] — resolve `TypeDef`s; [`DynSchema::encode`] /
//!   [`DynSchema::decode`] for one payload type.
//! - [`DynContract`] — a service's methods + schema; encode a method's
//!   request / decode its response by method name.
//! - [`encode_json`] / [`decode_json`] — stateless one-shot helpers.
//!
//! # Wire summary (what the encoder writes)
//!
//! `[0x01 xlang bitmap] [0xFF not-null] [type id] [type info] [data]`. Structs
//! are `NAMED_COMPATIBLE_STRUCT` (30) followed by a streamed TypeDef (index
//! marker, then the `TypeMeta` bytes on first use in the message, a back
//! reference after); enums `NAMED_ENUM` (26) + TypeDef then the ordinal;
//! unions `NAMED_UNION` (35) + TypeDef then the case id and the payload as a
//! ref-flagged, type-tagged slot. Field order inside a struct is Fory's:
//! non-nullable numerics, nullable numerics (fixed before varint, wider first,
//! type id, name), then everything else by snake-case name.
//!
//! # Decoding is wire-driven
//!
//! Compatible mode streams each struct's TypeDef, so the decoder reads the
//! sender's field list and types: fields the local contract lacks are kept
//! (appended after the declared ones), fields the sender lacks are filled with
//! their default (see [`DecodeOptions`]). The local schema names what the wire
//! omits — enum values and union cases.
//!
//! # What cannot be produced dynamically
//!
//! - `any`-typed fields (no static wire type) — rejected when the schema is
//!   built (`Unsupported`).
//! - `uuid` / `decimal` primitives — no Fory 1.7.6 xlang mapping any binding
//!   uses yet; rejected at schema build.
//! - Types registered by numeric id (`STRUCT`/`ENUM`/`EXT` + user id) and
//!   `EXT` types — Aster registers by name; decode fails with `Unsupported`.
//! - Shared/cyclic references — Aster runs with `ref=false`; a back-reference
//!   on the wire is `Unsupported`.
//! - String byte encoding: the encoder writes UTF-8 (as Rust does). pyfory
//!   and Java pick LATIN1/UTF-16 for narrow strings; select
//!   [`StringEncoding::Pyfory`] / [`StringEncoding::Latin1OrUtf16`] for
//!   byte-identity with them. Decoding accepts all three.
//! - Map/set iteration order: entries are written in the order given; typed
//!   `HashMap`/`HashSet` producers are not byte-stable themselves.

mod coerce;
mod contract;
mod decode;
mod encode;
mod json;
mod schema;
mod value;

pub use contract::{DynContract, DynMethod};
pub use json::JsonOptions;
pub use schema::{DynSchema, Prim};
pub use value::{DynEnum, DynError, DynErrorKind, DynResult, DynStruct, DynUnion, DynValue};

/// How the encoder writes strings. All three decode everywhere; they differ
/// only in which typed producer they are byte-identical to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StringEncoding {
    /// Always UTF-8 — Fory Rust (and Go).
    #[default]
    Utf8,
    /// LATIN1 when every char fits, else UTF-16 — Fory Java's compact strings.
    Latin1OrUtf16,
    /// LATIN1, else UTF-16 when every char is in the BMP, else UTF-8 —
    /// pyfory (follows CPython's string storage width).
    Pyfory,
}

/// What the encoder does with a declared field the value does not mention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissingFields {
    /// Use the declared default, `null` for optional fields, else the zero
    /// value — what a typed producer's default-constructed struct sends.
    #[default]
    Default,
    /// Fail when a `required` field without a declared default is missing
    /// (optional and defaulted fields are still filled).
    Error,
}

/// What the encoder does with a field the type does not declare.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnknownFields {
    /// Fail (catches typos).
    #[default]
    Error,
    /// Drop it.
    Ignore,
}

/// Encoder options.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeOptions {
    pub string_encoding: StringEncoding,
    pub missing_fields: MissingFields,
    pub unknown_fields: UnknownFields,
}

/// Decoder options.
#[derive(Clone, Debug)]
pub struct DecodeOptions {
    /// Fill declared fields the sender did not write (older producer) with
    /// their default — what a typed compatible-mode reader does. Default on.
    pub fill_missing: bool,
    /// Keep fields the sender wrote but the local schema does not declare
    /// (newer producer), after the declared fields. Default on.
    pub keep_unknown_fields: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        DecodeOptions {
            fill_missing: true,
            keep_unknown_fields: true,
        }
    }
}

impl DynSchema {
    /// Encode `value` as a `type_name` payload (coercing it first, see
    /// [`coerce`](Self::coerce)).
    pub fn encode(&self, type_name: &str, value: &DynValue) -> DynResult<Vec<u8>> {
        self.encode_with(type_name, value, &EncodeOptions::default())
    }

    /// [`encode`](Self::encode) with options.
    pub fn encode_with(
        &self,
        type_name: &str,
        value: &DynValue,
        opts: &EncodeOptions,
    ) -> DynResult<Vec<u8>> {
        let idx = self.require(type_name)?;
        let canonical = self.coerce_with(type_name, value, opts)?;
        let mut buf = Vec::with_capacity(64);
        encode::Encoder::new(self, &mut buf, opts)
            .write_root(idx, &canonical)
            .map_err(|e| e.at(&self.types[idx].full_name))?;
        Ok(buf)
    }

    /// Decode a payload; its root type comes from the wire.
    pub fn decode(&self, bytes: &[u8]) -> DynResult<DynValue> {
        self.decode_with(None, bytes, &DecodeOptions::default())
    }

    /// Decode a payload that must be a `type_name`.
    pub fn decode_as(&self, type_name: &str, bytes: &[u8]) -> DynResult<DynValue> {
        self.decode_with(Some(type_name), bytes, &DecodeOptions::default())
    }

    /// [`decode`](Self::decode) / [`decode_as`](Self::decode_as) with options.
    pub fn decode_with(
        &self,
        type_name: Option<&str>,
        bytes: &[u8],
        opts: &DecodeOptions,
    ) -> DynResult<DynValue> {
        let expect = match type_name {
            Some(n) => Some(self.require(n)?),
            None => None,
        };
        decode::Decoder::new(self, bytes, opts).read_root(expect)
    }

    /// JSON in, payload bytes out ([`DynValue::from_json`] + [`encode`](Self::encode)).
    pub fn encode_json(&self, type_name: &str, json: &[u8]) -> DynResult<Vec<u8>> {
        let v: serde_json::Value =
            serde_json::from_slice(json).map_err(|e| DynError::invalid(format!("JSON: {e}")))?;
        self.encode(type_name, &DynValue::from_json(&v))
    }

    /// Payload bytes in, JSON out ([`decode_with`](Self::decode_with) +
    /// [`DynValue::to_json`]).
    pub fn decode_json(&self, type_name: Option<&str>, bytes: &[u8]) -> DynResult<Vec<u8>> {
        let v = self.decode_with(type_name, bytes, &DecodeOptions::default())?;
        Ok(serde_json::to_vec(&v.to_json(&JsonOptions::default())).expect("JSON value serializes"))
    }
}

/// One-shot: encode JSON `value` as a `type_name` payload, the schema given as
/// a JSON array of `TypeDef`s (see [`DynSchema::from_json`]). For FFI / PyO3 /
/// NAPI wrappers that do not want to hold a schema handle.
pub fn encode_json(type_defs_json: &[u8], type_name: &str, value: &[u8]) -> DynResult<Vec<u8>> {
    let defs = std::str::from_utf8(type_defs_json)
        .map_err(|_| DynError::schema("type defs JSON is not UTF-8"))?;
    DynSchema::from_json(defs)?.encode_json(type_name, value)
}

/// One-shot: decode a payload to JSON (see [`encode_json`]). `type_name`
/// checks the root type when given.
pub fn decode_json(
    type_defs_json: &[u8],
    type_name: Option<&str>,
    payload: &[u8],
) -> DynResult<Vec<u8>> {
    let defs = std::str::from_utf8(type_defs_json)
        .map_err(|_| DynError::schema("type defs JSON is not UTF-8"))?;
    DynSchema::from_json(defs)?.decode_json(type_name, payload)
}
