//! [`DynValue`]: the schema-free value model the dynamic codec reads and writes.

use std::fmt;

pub use super::scalars::DynDecimal;

/// A dynamically typed payload value.
///
/// The variants mirror the Fory xlang wire types an Aster contract can name.
/// [`DynSchema::coerce`](super::DynSchema::coerce) turns a loosely typed value
/// (for example one built from JSON, where every integer is an `I64`, every
/// object a `Map` with string keys and every enum a `String`) into the exact
/// shape the target type needs. Decoding always yields the exact shape: integer
/// variants match the wire width (`varint32` -> `I32`, `var_uint64` -> `U64`,
/// ...), structs list every field, enums and unions carry their names when the
/// local schema knows them.
#[derive(Clone, Debug, PartialEq)]
pub enum DynValue {
    /// Absent value (an optional field, or a null map key/value).
    Null,
    Bool(bool),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    /// `float16` / `bfloat16` / `float32`.
    F32(f32),
    F64(f64),
    String(String),
    /// `binary`.
    Bytes(Vec<u8>),
    /// `timestamp`: seconds since the Unix epoch plus nanoseconds.
    Timestamp {
        seconds: i64,
        nanos: u32,
    },
    /// `date`: days since the Unix epoch.
    Date {
        days: i64,
    },
    /// `duration`.
    Duration {
        seconds: i64,
        nanos: i32,
    },
    /// `decimal` / `decimal(p,s)`: Fory DECIMAL (type id 40).
    Decimal(DynDecimal),
    /// `uuid`: 16 bytes (RFC 9562 order), Fory BINARY on the wire.
    Uuid([u8; 16]),
    /// `ulid`: 16 bytes, Fory BINARY on the wire; an identity distinct from
    /// `uuid`.
    Ulid([u8; 16]),
    List(Vec<DynValue>),
    Set(Vec<DynValue>),
    /// Map entries in wire order (keys may be any scalar).
    Map(Vec<(DynValue, DynValue)>),
    Struct(DynStruct),
    Enum(DynEnum),
    Union(DynUnion),
}

/// A message value.
#[derive(Clone, Debug, PartialEq)]
pub struct DynStruct {
    /// Fully qualified Fory name, `package.Name` (empty when unknown: a struct
    /// built from JSON names its type through the schema position).
    pub type_name: String,
    /// Fields in order. On decode: the local schema's declaration order, then
    /// any fields only the sender knows (compatible mode keeps them).
    pub fields: Vec<(String, DynValue)>,
}

impl DynStruct {
    /// A struct with no fields yet.
    pub fn new(type_name: impl Into<String>) -> Self {
        DynStruct {
            type_name: type_name.into(),
            fields: Vec::new(),
        }
    }

    /// Builder: append a field.
    pub fn with(mut self, name: impl Into<String>, value: DynValue) -> Self {
        self.fields.push((name.into(), value));
        self
    }

    /// The value of field `name`, if present.
    pub fn get(&self, name: &str) -> Option<&DynValue> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

/// An enum value. The wire carries only `ordinal` (the contract's
/// `EnumValueDef.value`); `name` is filled from the local schema on decode.
#[derive(Clone, Debug, PartialEq)]
pub struct DynEnum {
    pub type_name: String,
    pub name: Option<String>,
    pub ordinal: u32,
}

/// A union (data-carrying enum) value: one case and its payload.
#[derive(Clone, Debug, PartialEq)]
pub struct DynUnion {
    pub type_name: String,
    /// The Fory case id (the contract's `UnionVariantDef.id`).
    pub case_id: u32,
    /// The variant name, filled from the local schema on decode.
    pub case_name: Option<String>,
    pub value: Box<DynValue>,
}

impl DynValue {
    /// Shorthand for a struct value.
    pub fn structure(type_name: impl Into<String>, fields: Vec<(&str, DynValue)>) -> DynValue {
        DynValue::Struct(DynStruct {
            type_name: type_name.into(),
            fields: fields
                .into_iter()
                .map(|(n, v)| (n.to_string(), v))
                .collect(),
        })
    }

    /// Whether this is [`DynValue::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, DynValue::Null)
    }

    /// The struct, if this is one.
    pub fn as_struct(&self) -> Option<&DynStruct> {
        match self {
            DynValue::Struct(s) => Some(s),
            _ => None,
        }
    }

    /// A short name of the variant, for error messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            DynValue::Null => "null",
            DynValue::Bool(_) => "bool",
            DynValue::I8(_) => "i8",
            DynValue::I16(_) => "i16",
            DynValue::I32(_) => "i32",
            DynValue::I64(_) => "i64",
            DynValue::U8(_) => "u8",
            DynValue::U16(_) => "u16",
            DynValue::U32(_) => "u32",
            DynValue::U64(_) => "u64",
            DynValue::F32(_) => "f32",
            DynValue::F64(_) => "f64",
            DynValue::String(_) => "string",
            DynValue::Bytes(_) => "bytes",
            DynValue::Timestamp { .. } => "timestamp",
            DynValue::Date { .. } => "date",
            DynValue::Duration { .. } => "duration",
            DynValue::Decimal(_) => "decimal",
            DynValue::Uuid(_) => "uuid",
            DynValue::Ulid(_) => "ulid",
            DynValue::List(_) => "list",
            DynValue::Set(_) => "set",
            DynValue::Map(_) => "map",
            DynValue::Struct(_) => "struct",
            DynValue::Enum(_) => "enum",
            DynValue::Union(_) => "union",
        }
    }
}

/// What went wrong in the dynamic codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynErrorKind {
    /// The contract types are inconsistent (dangling reference, duplicate
    /// name, a field type the codec cannot express).
    Schema,
    /// A type name / hash / method the schema does not know.
    UnknownType,
    /// The value does not fit the type (wrong shape, out of range, missing
    /// required field, unknown field).
    InvalidValue,
    /// The bytes are not a valid payload.
    Decode,
    /// A Fory feature the dynamic codec does not implement.
    Unsupported,
}

/// A dynamic codec error: kind, the value path it happened at (for example
/// `interop.EchoRequest.items[2]`), and a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynError {
    pub kind: DynErrorKind,
    pub path: String,
    pub message: String,
}

impl DynError {
    pub(crate) fn new(kind: DynErrorKind, message: impl Into<String>) -> Self {
        DynError {
            kind,
            path: String::new(),
            message: message.into(),
        }
    }

    pub(crate) fn schema(message: impl Into<String>) -> Self {
        Self::new(DynErrorKind::Schema, message)
    }

    pub(crate) fn unknown(message: impl Into<String>) -> Self {
        Self::new(DynErrorKind::UnknownType, message)
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(DynErrorKind::InvalidValue, message)
    }

    pub(crate) fn decode(message: impl Into<String>) -> Self {
        Self::new(DynErrorKind::Decode, message)
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(DynErrorKind::Unsupported, message)
    }

    /// Prefix the path with an outer segment (`seg` is `.name` or `[i]`, or a
    /// type name at the root).
    pub(crate) fn at(mut self, seg: &str) -> Self {
        self.path = format!("{seg}{}", self.path);
        self
    }
}

impl fmt::Display for DynError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            DynErrorKind::Schema => "schema error",
            DynErrorKind::UnknownType => "unknown type",
            DynErrorKind::InvalidValue => "invalid value",
            DynErrorKind::Decode => "decode error",
            DynErrorKind::Unsupported => "unsupported",
        };
        if self.path.is_empty() {
            write!(f, "{kind}: {}", self.message)
        } else {
            write!(f, "{kind} at {}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for DynError {}

impl From<fory_core::Error> for DynError {
    fn from(e: fory_core::Error) -> Self {
        DynError::decode(e.to_string())
    }
}

/// Crate-local result alias.
pub type DynResult<T> = std::result::Result<T, DynError>;
