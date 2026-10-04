//! [`DynSchema`]: contract `TypeDef`s resolved into Fory wire types.

use std::collections::HashMap;

use fory_core::meta::{
    FieldInfo, FieldType, MetaString, TypeMeta, NAMESPACE_ENCODER, NAMESPACE_ENCODINGS,
    TYPE_NAME_ENCODER, TYPE_NAME_ENCODINGS,
};
use fory_core::type_id as tid;

use super::value::{DynError, DynResult, DynValue};
use crate::canonical;
use crate::contract::{
    canonical_xlang_bytes_type_def, compute_type_hash, ContainerKind, FieldDef, TypeDef,
    TypeDefKind, TypeKind,
};

/// A scalar wire type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prim {
    Bool,
    Int8,
    Int16,
    /// Fixed 4-byte `int32`.
    Int32,
    /// Zig-zag varint `varint32` (Rust `i32`, Python `pyfory.Int32`).
    VarInt32,
    Int64,
    VarInt64,
    TaggedInt64,
    UInt8,
    UInt16,
    UInt32,
    VarUInt32,
    UInt64,
    VarUInt64,
    TaggedUInt64,
    Float16,
    BFloat16,
    Float32,
    Float64,
    String,
    Binary,
    Timestamp,
    Date,
    Duration,
}

impl Prim {
    /// Parse a contract `type_primitive` (the canonical names every binding
    /// emits, plus a few lenient aliases).
    pub fn from_contract_name(name: &str) -> Option<Prim> {
        Some(match name {
            "bool" | "boolean" => Prim::Bool,
            "int8" | "i8" => Prim::Int8,
            "int16" | "i16" => Prim::Int16,
            "int32" | "fixed_int32" => Prim::Int32,
            "varint32" | "i32" => Prim::VarInt32,
            "int64" | "fixed_int64" => Prim::Int64,
            "varint64" | "i64" | "int" => Prim::VarInt64,
            "tagged_int64" => Prim::TaggedInt64,
            "uint8" | "u8" => Prim::UInt8,
            "uint16" | "u16" => Prim::UInt16,
            "uint32" | "fixed_uint32" => Prim::UInt32,
            "var_uint32" | "u32" => Prim::VarUInt32,
            "uint64" | "fixed_uint64" => Prim::UInt64,
            "var_uint64" | "u64" => Prim::VarUInt64,
            "tagged_uint64" => Prim::TaggedUInt64,
            "float16" => Prim::Float16,
            "bfloat16" => Prim::BFloat16,
            "float32" | "f32" => Prim::Float32,
            "float64" | "f64" | "float" | "double" => Prim::Float64,
            "string" | "str" => Prim::String,
            "binary" | "bytes" => Prim::Binary,
            "timestamp" => Prim::Timestamp,
            "date" => Prim::Date,
            "duration" => Prim::Duration,
            _ => return None,
        })
    }

    /// The canonical contract name.
    pub fn contract_name(self) -> &'static str {
        match self {
            Prim::Bool => "bool",
            Prim::Int8 => "int8",
            Prim::Int16 => "int16",
            Prim::Int32 => "int32",
            Prim::VarInt32 => "varint32",
            Prim::Int64 => "int64",
            Prim::VarInt64 => "varint64",
            Prim::TaggedInt64 => "tagged_int64",
            Prim::UInt8 => "uint8",
            Prim::UInt16 => "uint16",
            Prim::UInt32 => "uint32",
            Prim::VarUInt32 => "var_uint32",
            Prim::UInt64 => "uint64",
            Prim::VarUInt64 => "var_uint64",
            Prim::TaggedUInt64 => "tagged_uint64",
            Prim::Float16 => "float16",
            Prim::BFloat16 => "bfloat16",
            Prim::Float32 => "float32",
            Prim::Float64 => "float64",
            Prim::String => "string",
            Prim::Binary => "binary",
            Prim::Timestamp => "timestamp",
            Prim::Date => "date",
            Prim::Duration => "duration",
        }
    }

    /// The Fory internal type id.
    pub fn type_id(self) -> u32 {
        match self {
            Prim::Bool => tid::BOOL,
            Prim::Int8 => tid::INT8,
            Prim::Int16 => tid::INT16,
            Prim::Int32 => tid::INT32,
            Prim::VarInt32 => tid::VARINT32,
            Prim::Int64 => tid::INT64,
            Prim::VarInt64 => tid::VARINT64,
            Prim::TaggedInt64 => tid::TAGGED_INT64,
            Prim::UInt8 => tid::UINT8,
            Prim::UInt16 => tid::UINT16,
            Prim::UInt32 => tid::UINT32,
            Prim::VarUInt32 => tid::VAR_UINT32,
            Prim::UInt64 => tid::UINT64,
            Prim::VarUInt64 => tid::VAR_UINT64,
            Prim::TaggedUInt64 => tid::TAGGED_UINT64,
            Prim::Float16 => tid::FLOAT16,
            Prim::BFloat16 => tid::BFLOAT16,
            Prim::Float32 => tid::FLOAT32,
            Prim::Float64 => tid::FLOAT64,
            Prim::String => tid::STRING,
            Prim::Binary => tid::BINARY,
            Prim::Timestamp => tid::TIMESTAMP,
            Prim::Date => tid::DATE,
            Prim::Duration => tid::DURATION,
        }
    }

    /// The scalar for a Fory internal type id.
    pub fn from_type_id(id: u32) -> Option<Prim> {
        Some(match id {
            tid::BOOL => Prim::Bool,
            tid::INT8 => Prim::Int8,
            tid::INT16 => Prim::Int16,
            tid::INT32 => Prim::Int32,
            tid::VARINT32 => Prim::VarInt32,
            tid::INT64 => Prim::Int64,
            tid::VARINT64 => Prim::VarInt64,
            tid::TAGGED_INT64 => Prim::TaggedInt64,
            tid::UINT8 => Prim::UInt8,
            tid::UINT16 => Prim::UInt16,
            tid::UINT32 => Prim::UInt32,
            tid::VAR_UINT32 => Prim::VarUInt32,
            tid::UINT64 => Prim::UInt64,
            tid::VAR_UINT64 => Prim::VarUInt64,
            tid::TAGGED_UINT64 => Prim::TaggedUInt64,
            tid::FLOAT16 => Prim::Float16,
            tid::BFLOAT16 => Prim::BFloat16,
            tid::FLOAT32 => Prim::Float32,
            tid::FLOAT64 => Prim::Float64,
            tid::STRING => Prim::String,
            tid::BINARY => Prim::Binary,
            tid::TIMESTAMP => Prim::Timestamp,
            tid::DATE => Prim::Date,
            tid::DURATION => Prim::Duration,
            _ => return None,
        })
    }

    /// Numeric/boolean scalars: the field-order "primitive" groups.
    fn is_primitive_group(self) -> bool {
        !matches!(
            self,
            Prim::String | Prim::Binary | Prim::Timestamp | Prim::Date | Prim::Duration
        )
    }

    /// Variable-length (compressed) integer encodings sort after fixed ones.
    fn is_compressed(self) -> bool {
        matches!(
            self,
            Prim::VarInt32
                | Prim::VarInt64
                | Prim::TaggedInt64
                | Prim::VarUInt32
                | Prim::VarUInt64
                | Prim::TaggedUInt64
        )
    }

    fn size(self) -> i32 {
        match self {
            Prim::Bool | Prim::Int8 | Prim::UInt8 => 1,
            Prim::Int16 | Prim::UInt16 | Prim::Float16 | Prim::BFloat16 => 2,
            Prim::Int32 | Prim::VarInt32 | Prim::UInt32 | Prim::VarUInt32 | Prim::Float32 => 4,
            Prim::Int64
            | Prim::VarInt64
            | Prim::TaggedInt64
            | Prim::UInt64
            | Prim::VarUInt64
            | Prim::TaggedUInt64
            | Prim::Float64 => 8,
            _ => 0,
        }
    }
}

/// The resolved wire type of a field, list element, map key/value or union
/// payload.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    Prim(Prim),
    /// Index into [`DynSchema::types`] (a message).
    Struct(usize),
    /// Index into [`DynSchema::types`] (an enum).
    Enum(usize),
    /// Index into [`DynSchema::types`] (a union).
    Union(usize),
    List(Box<FieldTy>),
    Set(Box<FieldTy>),
    Map(Box<FieldTy>, Box<FieldTy>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FieldTy {
    pub kind: Kind,
    pub nullable: bool,
}

impl FieldTy {
    fn new(kind: Kind, nullable: bool) -> Self {
        FieldTy { kind, nullable }
    }

    /// The Fory `FieldType` this field puts in a TypeDef.
    fn fory_field_type(&self) -> FieldType {
        let (id, generics) = match &self.kind {
            Kind::Prim(p) => (p.type_id(), vec![]),
            Kind::Struct(_) => (tid::NAMED_COMPATIBLE_STRUCT, vec![]),
            Kind::Enum(_) => (tid::ENUM, vec![]),
            Kind::Union(_) => (tid::UNION, vec![]),
            Kind::List(e) => (tid::LIST, vec![e.fory_field_type()]),
            Kind::Set(e) => (tid::SET, vec![e.fory_field_type()]),
            Kind::Map(k, v) => (tid::MAP, vec![k.fory_field_type(), v.fory_field_type()]),
        };
        FieldType::new(id, self.nullable, generics)
    }
}

/// One message field.
#[derive(Clone, Debug)]
pub(crate) struct SField {
    /// Name as declared in the contract.
    pub name: String,
    /// Fory wire name (`snake_case`).
    pub wire_name: String,
    pub ty: FieldTy,
    /// Declared default (contract `default_value`), already decoded.
    pub default: Option<DynValue>,
}

#[derive(Clone, Debug)]
pub(crate) struct SVariant {
    pub name: String,
    pub case_id: u32,
    pub payload: FieldTy,
}

#[derive(Clone, Debug)]
pub(crate) enum Body {
    Message {
        /// Declaration order.
        fields: Vec<SField>,
        /// Indices into `fields` in Fory wire order.
        wire_order: Vec<usize>,
    },
    Enum {
        /// `(name, ordinal)` in declaration order.
        values: Vec<(String, u32)>,
    },
    Union {
        variants: Vec<SVariant>,
    },
}

/// One contract type resolved for the wire.
#[derive(Clone, Debug)]
pub(crate) struct SType {
    pub def: TypeDef,
    pub hash: [u8; 32],
    /// `package.Name` — the Fory registration name.
    pub full_name: String,
    pub namespace: String,
    pub name: String,
    pub body: Body,
    /// The streamed TypeDef bytes (`TypeMeta` header + body) for this type.
    pub type_meta: Vec<u8>,
}

impl SType {
    /// The Fory type id written before a value of this type.
    pub fn wire_type_id(&self) -> u32 {
        match self.body {
            Body::Message { .. } => tid::NAMED_COMPATIBLE_STRUCT,
            Body::Enum { .. } => tid::NAMED_ENUM,
            Body::Union { .. } => tid::NAMED_UNION,
        }
    }

    pub fn field_by_name(&self, name: &str) -> Option<(usize, &SField)> {
        match &self.body {
            Body::Message { fields, .. } => fields
                .iter()
                .enumerate()
                .find(|(_, f)| f.name == name || f.wire_name == name),
            _ => None,
        }
    }
}

/// Contract types resolved into Fory xlang wire types — the input of the
/// dynamic encoder and the name source of the decoder.
///
/// Build it from every `TypeDef` a contract publishes (`types/{hash}.bin`).
/// Types are found by `package.Name`, `package/Name` (the Aster wire tag), the
/// bare `Name` when unique, or the 64-hex type hash. `aster/Empty` (the
/// implicit request of a no-argument unary method) is always present.
#[derive(Clone, Debug)]
pub struct DynSchema {
    pub(crate) types: Vec<SType>,
    lookup: HashMap<String, usize>,
    /// `(namespace, name)` → index, for decode.
    by_fory: HashMap<(String, String), usize>,
}

/// Fields the snake-casing of Fory xlang applies to names.
pub(crate) fn wire_field_name(name: &str) -> String {
    fory_core::util::to_snake_case(name)
}

fn empty_type_def() -> TypeDef {
    TypeDef {
        kind: TypeDefKind::Message,
        package: "aster".into(),
        name: "Empty".into(),
        fields: vec![],
        enum_values: vec![],
        union_variants: vec![],
    }
}

fn meta_names(namespace: &str, name: &str) -> DynResult<(MetaString, MetaString)> {
    let ns = NAMESPACE_ENCODER
        .encode_with_encodings(namespace, NAMESPACE_ENCODINGS)
        .map_err(|e| DynError::schema(format!("namespace {namespace:?}: {e}")))?;
    let tn = TYPE_NAME_ENCODER
        .encode_with_encodings(name, TYPE_NAME_ENCODINGS)
        .map_err(|e| DynError::schema(format!("type name {name:?}: {e}")))?;
    Ok((ns, tn))
}

/// Fory registration name of a type: `package.Name`, or `Name` with no package.
fn fory_full_name(td: &TypeDef) -> String {
    if td.package.is_empty() {
        td.name.clone()
    } else {
        format!("{}.{}", td.package, td.name)
    }
}

impl DynSchema {
    /// Resolve a set of contract `TypeDef`s. Every `Ref` must point at a type
    /// in the set (by hash); `SelfRef` names a type in the set by
    /// `package.Name` / `package/Name` / `Name`.
    pub fn from_type_defs<I: IntoIterator<Item = TypeDef>>(defs: I) -> DynResult<DynSchema> {
        let mut schema = DynSchema {
            types: Vec::new(),
            lookup: HashMap::new(),
            by_fory: HashMap::new(),
        };
        let mut all: Vec<TypeDef> = defs.into_iter().collect();
        if !all
            .iter()
            .any(|t| t.package == "aster" && t.name == "Empty")
        {
            all.push(empty_type_def());
        }

        // Pass 1: identities.
        let mut by_hash: HashMap<[u8; 32], usize> = HashMap::new();
        for td in all {
            let hash = compute_type_hash(&canonical_xlang_bytes_type_def(&td));
            if by_hash.contains_key(&hash) {
                continue; // the same type twice
            }
            let full = fory_full_name(&td);
            let (namespace, name) = match full.rsplit_once('.') {
                Some((ns, n)) => (ns.to_string(), n.to_string()),
                None => (String::new(), full.clone()),
            };
            if schema
                .by_fory
                .contains_key(&(namespace.clone(), name.clone()))
            {
                return Err(DynError::schema(format!(
                    "two different types share the wire name {full}"
                )));
            }
            let idx = schema.types.len();
            by_hash.insert(hash, idx);
            schema
                .by_fory
                .insert((namespace.clone(), name.clone()), idx);
            schema.types.push(SType {
                def: td,
                hash,
                full_name: full,
                namespace,
                name,
                body: Body::Enum { values: vec![] },
                type_meta: Vec::new(),
            });
        }
        for (idx, t) in schema.types.iter().enumerate() {
            schema.lookup.insert(t.full_name.clone(), idx);
            schema
                .lookup
                .insert(format!("{}/{}", t.def.package, t.def.name).to_string(), idx);
            schema.lookup.insert(hex::encode(t.hash), idx);
            if t.def.package.is_empty() {
                schema.lookup.insert(t.def.name.clone(), idx);
            }
        }
        // Bare names, when unambiguous.
        let mut bare: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, t) in schema.types.iter().enumerate() {
            bare.entry(t.def.name.clone()).or_default().push(idx);
        }
        for (n, idxs) in bare {
            if idxs.len() == 1 {
                schema.lookup.entry(n).or_insert(idxs[0]);
            }
        }

        // Pass 2: bodies.
        for idx in 0..schema.types.len() {
            let body = schema
                .resolve_body(&schema.types[idx].def, &by_hash)
                .map_err(|e| e.at(&schema.types[idx].full_name))?;
            schema.types[idx].body = body;
        }

        // Pass 3: streamed TypeDefs.
        for idx in 0..schema.types.len() {
            let meta = schema
                .build_type_meta(idx)
                .map_err(|e| e.at(&schema.types[idx].full_name))?;
            schema.types[idx].type_meta = meta;
        }
        Ok(schema)
    }

    /// [`from_type_defs`](Self::from_type_defs) from canonical `TypeDef`
    /// bytes (the `types/{hash}.bin` blobs of a contract collection).
    pub fn from_canonical_type_defs<'a, I>(blobs: I) -> DynResult<DynSchema>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let mut defs = Vec::new();
        for b in blobs {
            defs.push(
                crate::contract::decode_type_def_bytes(b)
                    .map_err(|e| DynError::schema(format!("TypeDef bytes: {e}")))?,
            );
        }
        Self::from_type_defs(defs)
    }

    /// [`from_type_defs`](Self::from_type_defs) from JSON: an array of
    /// `TypeDef` objects (the shape `canonical_bytes_to_json("TypeDef", ..)`
    /// produces), or an object `{"types": [...]}`.
    pub fn from_json(json: &str) -> DynResult<DynSchema> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| DynError::schema(format!("type defs JSON: {e}")))?;
        let arr = match v {
            serde_json::Value::Array(a) => a,
            serde_json::Value::Object(mut o) => match o.remove("types") {
                Some(serde_json::Value::Array(a)) => a,
                _ => {
                    return Err(DynError::schema(
                        "expected a TypeDef array or {\"types\": [...]}",
                    ))
                }
            },
            _ => {
                return Err(DynError::schema(
                    "expected a TypeDef array or {\"types\": [...]}",
                ))
            }
        };
        let mut defs = Vec::with_capacity(arr.len());
        for (i, item) in arr.into_iter().enumerate() {
            defs.push(
                serde_json::from_value::<TypeDef>(item)
                    .map_err(|e| DynError::schema(format!("types[{i}]: {e}")))?,
            );
        }
        Self::from_type_defs(defs)
    }

    /// Find a type by `package.Name`, `package/Name`, unique bare `Name`, or
    /// 64-hex type hash. Returns its `package.Name`.
    pub fn resolve(&self, name: &str) -> Option<&str> {
        self.index_of(name)
            .map(|i| self.types[i].full_name.as_str())
    }

    /// The `TypeDef` of a type (any accepted name form).
    pub fn type_def(&self, name: &str) -> Option<&TypeDef> {
        self.index_of(name).map(|i| &self.types[i].def)
    }

    /// The 32-byte contract hash of a type.
    pub fn type_hash(&self, name: &str) -> Option<[u8; 32]> {
        self.index_of(name).map(|i| self.types[i].hash)
    }

    /// All types, as `package.Name`.
    pub fn type_names(&self) -> Vec<&str> {
        self.types.iter().map(|t| t.full_name.as_str()).collect()
    }

    /// The streamed Fory TypeDef bytes (`TypeMeta` header + body) the encoder
    /// writes for a type — exactly what a typed Fory 1.7.6 runtime writes for
    /// the same registration.
    pub fn type_meta_bytes(&self, name: &str) -> Option<&[u8]> {
        self.index_of(name)
            .map(|i| self.types[i].type_meta.as_slice())
    }

    /// The Fory wire field order of a message type (snake-case wire names).
    pub fn wire_field_order(&self, name: &str) -> Option<Vec<&str>> {
        let t = &self.types[self.index_of(name)?];
        match &t.body {
            Body::Message { fields, wire_order } => Some(
                wire_order
                    .iter()
                    .map(|&i| fields[i].wire_name.as_str())
                    .collect(),
            ),
            _ => None,
        }
    }

    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        if let Some(i) = self.lookup.get(name) {
            return Some(*i);
        }
        let lower = name.to_ascii_lowercase();
        if lower.len() == 64 && lower != name {
            return self.lookup.get(&lower).copied();
        }
        None
    }

    pub(crate) fn index_by_fory(&self, namespace: &str, name: &str) -> Option<usize> {
        self.by_fory
            .get(&(namespace.to_string(), name.to_string()))
            .copied()
    }

    pub(crate) fn require(&self, name: &str) -> DynResult<usize> {
        self.index_of(name)
            .ok_or_else(|| DynError::unknown(format!("type {name:?} is not in the schema")))
    }

    fn resolve_body(&self, td: &TypeDef, by_hash: &HashMap<[u8; 32], usize>) -> DynResult<Body> {
        match td.kind {
            TypeDefKind::Message => {
                let mut fields = Vec::with_capacity(td.fields.len());
                let mut seen = std::collections::HashSet::new();
                for fd in &td.fields {
                    let f = self
                        .resolve_field(fd, by_hash)
                        .map_err(|e| e.at(&format!(".{}", fd.name)))?;
                    if !seen.insert(f.wire_name.clone()) {
                        return Err(DynError::schema(format!(
                            "duplicate field wire name {:?}",
                            f.wire_name
                        )));
                    }
                    fields.push(f);
                }
                let wire_order = wire_order(&fields);
                Ok(Body::Message { fields, wire_order })
            }
            TypeDefKind::Enum => {
                let mut values = Vec::with_capacity(td.enum_values.len());
                for ev in &td.enum_values {
                    let ord = u32::try_from(ev.value).map_err(|_| {
                        DynError::schema(format!(
                            "enum value {} = {} is negative; Fory enum ordinals are unsigned",
                            ev.name, ev.value
                        ))
                    })?;
                    values.push((ev.name.clone(), ord));
                }
                Ok(Body::Enum { values })
            }
            TypeDefKind::Union => {
                let mut variants = Vec::with_capacity(td.union_variants.len());
                for uv in &td.union_variants {
                    let case_id = u32::try_from(uv.id).map_err(|_| {
                        DynError::schema(format!(
                            "union case {} has negative id {}",
                            uv.name, uv.id
                        ))
                    })?;
                    let kind = self
                        .ref_kind(&uv.type_ref, by_hash)
                        .map_err(|e| e.at(&format!(".{}", uv.name)))?;
                    variants.push(SVariant {
                        name: uv.name.clone(),
                        case_id,
                        payload: FieldTy::new(kind, false),
                    });
                }
                Ok(Body::Union { variants })
            }
        }
    }

    fn ref_kind(&self, hex_ref: &str, by_hash: &HashMap<[u8; 32], usize>) -> DynResult<Kind> {
        let bytes = hex::decode(hex_ref)
            .map_err(|_| DynError::schema(format!("type_ref {hex_ref:?} is not hex")))?;
        let hash: [u8; 32] = bytes
            .try_into()
            .map_err(|_| DynError::schema(format!("type_ref {hex_ref:?} is not 32 bytes")))?;
        let idx = *by_hash.get(&hash).ok_or_else(|| {
            DynError::schema(format!(
                "type_ref {hex_ref} names a type that is not in the schema"
            ))
        })?;
        Ok(self.kind_of_index(idx))
    }

    fn kind_of_index(&self, idx: usize) -> Kind {
        match self.types[idx].def.kind {
            TypeDefKind::Message => Kind::Struct(idx),
            TypeDefKind::Enum => Kind::Enum(idx),
            TypeDefKind::Union => Kind::Union(idx),
        }
    }

    fn leaf_kind(
        &self,
        kind: TypeKind,
        primitive: &str,
        type_ref: &str,
        self_ref_name: &str,
        by_hash: &HashMap<[u8; 32], usize>,
    ) -> DynResult<Kind> {
        match kind {
            TypeKind::Primitive => Prim::from_contract_name(primitive)
                .map(Kind::Prim)
                .ok_or_else(|| {
                    DynError::unsupported(format!(
                        "primitive {primitive:?} has no dynamic Fory mapping"
                    ))
                }),
            TypeKind::Ref => self.ref_kind(type_ref, by_hash),
            TypeKind::SelfRef => {
                let idx = self
                    .lookup
                    .get(self_ref_name)
                    .copied()
                    .or_else(|| {
                        // Python spells self-refs `module.Class`; fall back
                        // to the trailing name.
                        let tail = self_ref_name.rsplit(['.', '/']).next().unwrap_or("");
                        self.lookup.get(tail).copied()
                    })
                    .ok_or_else(|| {
                        DynError::schema(format!(
                            "self_ref {self_ref_name:?} names no type in the schema"
                        ))
                    })?;
                Ok(self.kind_of_index(idx))
            }
            TypeKind::Any => Err(DynError::unsupported(
                "an `any` field has no static wire type; the dynamic codec cannot encode it",
            )),
        }
    }

    fn resolve_field(
        &self,
        fd: &FieldDef,
        by_hash: &HashMap<[u8; 32], usize>,
    ) -> DynResult<SField> {
        let leaf = self.leaf_kind(
            fd.type_kind,
            &fd.type_primitive,
            &fd.type_ref,
            &fd.self_ref_name,
            by_hash,
        )?;
        let elem = FieldTy::new(leaf, false);
        let kind = match fd.container {
            ContainerKind::None => elem.kind,
            ContainerKind::List => Kind::List(Box::new(elem)),
            ContainerKind::Set => Kind::Set(Box::new(elem)),
            ContainerKind::Map => {
                let key = self.leaf_kind(
                    fd.container_key_kind,
                    &fd.container_key_primitive,
                    &fd.container_key_ref,
                    "",
                    by_hash,
                )?;
                Kind::Map(Box::new(FieldTy::new(key, false)), Box::new(elem))
            }
        };
        let ty = FieldTy::new(kind, fd.optional);
        let default = if fd.required || fd.default_value.is_empty() {
            None
        } else {
            Some(parse_default(&ty, &fd.default_value)?)
        };
        Ok(SField {
            name: fd.name.clone(),
            wire_name: wire_field_name(&fd.name),
            ty,
            default,
        })
    }

    fn build_type_meta(&self, idx: usize) -> DynResult<Vec<u8>> {
        let t = &self.types[idx];
        let (ns, tn) = meta_names(&t.namespace, &t.name)?;
        let (type_id, infos) = match &t.body {
            Body::Message { fields, wire_order } => (
                tid::NAMED_COMPATIBLE_STRUCT,
                wire_order
                    .iter()
                    .map(|&i| FieldInfo::new(&fields[i].wire_name, fields[i].ty.fory_field_type()))
                    .collect(),
            ),
            Body::Enum { .. } => (tid::NAMED_ENUM, vec![]),
            Body::Union { .. } => (tid::NAMED_UNION, vec![]),
        };
        let meta = TypeMeta::new(type_id, u32::MAX, ns, tn, true, infos)
            .map_err(|e| DynError::schema(format!("TypeMeta: {e}")))?;
        Ok(meta.get_bytes().to_vec())
    }
}

/// Fory field order, as `fory-derive` 1.7.6 and pyfory 1.7 compute it:
/// non-nullable numeric/bool fields, then nullable ones (each by fixed-before-
/// varint, size descending, type id ascending, name), then every other field
/// by snake-case name.
fn wire_order(fields: &[SField]) -> Vec<usize> {
    let mut prim = Vec::new();
    let mut nullable_prim = Vec::new();
    let mut rest = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        match &f.ty.kind {
            Kind::Prim(p) if p.is_primitive_group() => {
                if f.ty.nullable {
                    nullable_prim.push(i)
                } else {
                    prim.push(i)
                }
            }
            _ => rest.push(i),
        }
    }
    let numeric = |a: &usize, b: &usize| {
        let (fa, fb) = (&fields[*a], &fields[*b]);
        let (Kind::Prim(pa), Kind::Prim(pb)) = (&fa.ty.kind, &fb.ty.kind) else {
            unreachable!()
        };
        pa.is_compressed()
            .cmp(&pb.is_compressed())
            .then_with(|| pb.size().cmp(&pa.size()))
            .then_with(|| pa.type_id().cmp(&pb.type_id()))
            .then_with(|| fa.wire_name.cmp(&fb.wire_name))
    };
    prim.sort_by(numeric);
    nullable_prim.sort_by(numeric);
    rest.sort_by(|a, b| fields[*a].wire_name.cmp(&fields[*b].wire_name));
    prim.extend(nullable_prim);
    prim.extend(rest);
    prim
}

/// Decode a contract `default_value` (hex of the canonical encoding,
/// Aster-ContractIdentity §11.3.2.3) into a value of the field's type.
fn parse_default(ty: &FieldTy, hex_value: &str) -> DynResult<DynValue> {
    let bytes = hex::decode(hex_value)
        .map_err(|_| DynError::schema(format!("default_value {hex_value:?} is not hex")))?;
    let bad = |e: anyhow::Error| DynError::schema(format!("default_value: {e}"));
    let mut pos = 0usize;
    let v = match &ty.kind {
        Kind::List(_) | Kind::Set(_) | Kind::Map(..) => {
            if bytes != [0u8] {
                return Err(DynError::schema(
                    "a container default must be the empty sentinel 00",
                ));
            }
            return Ok(match &ty.kind {
                Kind::List(_) => DynValue::List(vec![]),
                Kind::Set(_) => DynValue::Set(vec![]),
                _ => DynValue::Map(vec![]),
            });
        }
        Kind::Prim(Prim::String) => DynValue::String(canonical::read_string(&bytes, &mut pos).map_err(bad)?),
        Kind::Prim(Prim::Bool) => DynValue::Bool(canonical::read_bool(&bytes, &mut pos).map_err(bad)?),
        Kind::Prim(Prim::VarInt32) | Kind::Prim(Prim::Int32) => {
            DynValue::I32(canonical::read_zigzag_i32(&bytes, &mut pos).map_err(bad)?)
        }
        Kind::Prim(Prim::VarInt64) | Kind::Prim(Prim::Int64) | Kind::Prim(Prim::TaggedInt64) => {
            DynValue::I64(canonical::read_zigzag_i64(&bytes, &mut pos).map_err(bad)?)
        }
        Kind::Prim(Prim::Float64) => DynValue::F64(canonical::read_float64(&bytes, &mut pos).map_err(bad)?),
        _ => {
            return Err(DynError::unsupported(
                "contract defaults are only defined for string/bool/varint32/varint64/float64 and empty containers",
            ))
        }
    };
    if pos != bytes.len() {
        return Err(DynError::schema("default_value has trailing bytes"));
    }
    Ok(v)
}
