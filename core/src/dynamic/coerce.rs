//! Coercion: a loosely typed [`DynValue`] (e.g. from JSON) → the exact shape a
//! schema type needs, with defaults applied.

use super::scalars::{self, DynDecimal};
use super::schema::{Body, DynSchema, FieldTy, Kind, Prim, SType};
use super::value::{DynEnum, DynError, DynResult, DynStruct, DynUnion, DynValue};
use super::{EncodeOptions, MissingFields, UnknownFields};

/// Deepest struct/collection nesting the codec follows.
pub(crate) const MAX_DEPTH: u32 = 128;

impl DynSchema {
    /// Coerce `value` into the exact typed shape of `type_name`: every struct
    /// lists all its fields in declaration order (missing ones filled from the
    /// declared default, `null` for optional fields, or the zero value — see
    /// [`MissingFields`]), integers take the variant of their wire width,
    /// enums carry their ordinal, unions their case id.
    ///
    /// Accepted loose forms: any integer variant (range-checked), integral
    /// floats and decimal strings for integers; numbers and `"NaN"` /
    /// `"Infinity"` / `"-Infinity"` for floats; base64 (standard or URL-safe,
    /// padded or not) or a list of bytes for `binary`; ISO-8601 strings for
    /// `timestamp` / `date`; a `Map` with string keys for a struct; a `List`
    /// of `[key, value]` pairs or a `Map` for a map; an enum's name or
    /// ordinal; a union as `{"_type": <variant name | payload type>, ...}` or
    /// `{"<variant>": payload}`.
    pub fn coerce(&self, type_name: &str, value: &DynValue) -> DynResult<DynValue> {
        self.coerce_with(type_name, value, &EncodeOptions::default())
    }

    /// [`coerce`](Self::coerce) with explicit options.
    pub fn coerce_with(
        &self,
        type_name: &str,
        value: &DynValue,
        opts: &EncodeOptions,
    ) -> DynResult<DynValue> {
        let idx = self.require(type_name)?;
        let ty = FieldTy {
            kind: self.kind_of(idx),
            nullable: false,
        };
        let t = &self.types[idx];
        self.coerce_ty(&ty, value, opts, 0)
            .map_err(|e| e.at(&t.full_name))
    }

    pub(crate) fn kind_of(&self, idx: usize) -> Kind {
        match self.types[idx].body {
            Body::Message { .. } => Kind::Struct(idx),
            Body::Enum { .. } => Kind::Enum(idx),
            Body::Union { .. } => Kind::Union(idx),
        }
    }

    pub(crate) fn coerce_ty(
        &self,
        ty: &FieldTy,
        v: &DynValue,
        opts: &EncodeOptions,
        depth: u32,
    ) -> DynResult<DynValue> {
        if depth > MAX_DEPTH {
            return Err(DynError::invalid("value nests too deeply"));
        }
        if v.is_null() {
            return if ty.nullable {
                Ok(DynValue::Null)
            } else {
                Err(DynError::invalid("null for a non-optional value"))
            };
        }
        match &ty.kind {
            Kind::Prim(p) => coerce_prim(*p, v),
            Kind::List(e) | Kind::Set(e) => {
                let items = match v {
                    DynValue::List(items) | DynValue::Set(items) => items,
                    other => {
                        return Err(DynError::invalid(format!(
                            "expected a list, got {}",
                            other.kind_name()
                        )))
                    }
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    out.push(
                        self.coerce_ty(e, item, opts, depth + 1)
                            .map_err(|err| err.at(&format!("[{i}]")))?,
                    );
                }
                Ok(if matches!(ty.kind, Kind::Set(_)) {
                    DynValue::Set(out)
                } else {
                    DynValue::List(out)
                })
            }
            Kind::Map(k, val) => {
                let pairs: Vec<(DynValue, &DynValue)> = match v {
                    DynValue::Map(entries) => entries.iter().map(|(a, b)| (a.clone(), b)).collect(),
                    DynValue::Struct(s) => s
                        .fields
                        .iter()
                        .map(|(n, b)| (DynValue::String(n.clone()), b))
                        .collect(),
                    DynValue::List(items) => {
                        let mut out = Vec::with_capacity(items.len());
                        for (i, item) in items.iter().enumerate() {
                            match item {
                                DynValue::List(kv) if kv.len() == 2 => {
                                    out.push((kv[0].clone(), &kv[1]))
                                }
                                _ => {
                                    return Err(DynError::invalid(
                                        "a map given as a list needs [key, value] pairs",
                                    )
                                    .at(&format!("[{i}]")))
                                }
                            }
                        }
                        out
                    }
                    other => {
                        return Err(DynError::invalid(format!(
                            "expected a map, got {}",
                            other.kind_name()
                        )))
                    }
                };
                let mut out = Vec::with_capacity(pairs.len());
                for (i, (key, value)) in pairs.into_iter().enumerate() {
                    let ck = self
                        .coerce_ty(k, &key, opts, depth + 1)
                        .map_err(|e| e.at(&format!("{{key {i}}}")))?;
                    let cv = self
                        .coerce_ty(val, value, opts, depth + 1)
                        .map_err(|e| e.at(&format!("[{}]", key_label(&key, i))))?;
                    out.push((ck, cv));
                }
                Ok(DynValue::Map(out))
            }
            Kind::Struct(idx) => self.coerce_struct(*idx, v, opts, depth),
            Kind::Enum(idx) => self.coerce_enum(*idx, v),
            Kind::Union(idx) => self.coerce_union(*idx, v, opts, depth),
        }
    }

    fn coerce_struct(
        &self,
        idx: usize,
        v: &DynValue,
        opts: &EncodeOptions,
        depth: u32,
    ) -> DynResult<DynValue> {
        let t = &self.types[idx];
        let Body::Message { fields, .. } = &t.body else {
            unreachable!()
        };
        let input: Vec<(&str, &DynValue)> = match v {
            DynValue::Struct(s) => {
                if !s.type_name.is_empty() && self.index_of(&s.type_name) != Some(idx) {
                    return Err(DynError::invalid(format!(
                        "expected {}, got a {} struct",
                        t.full_name, s.type_name
                    )));
                }
                s.fields.iter().map(|(n, v)| (n.as_str(), v)).collect()
            }
            DynValue::Map(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    match k {
                        DynValue::String(s) => out.push((s.as_str(), v)),
                        _ => return Err(DynError::invalid("struct field names must be strings")),
                    }
                }
                out
            }
            other => {
                return Err(DynError::invalid(format!(
                    "expected a {} struct, got {}",
                    t.full_name,
                    other.kind_name()
                )))
            }
        };
        let mut used = vec![false; input.len()];
        let mut out = Vec::with_capacity(fields.len());
        for f in fields {
            let found = input
                .iter()
                .position(|(n, _)| *n == f.name)
                .or_else(|| input.iter().position(|(n, _)| *n == f.wire_name));
            let value = match found {
                Some(i) => {
                    if used[i] {
                        return Err(DynError::invalid(format!("field {} given twice", f.name)));
                    }
                    used[i] = true;
                    self.coerce_ty(&f.ty, input[i].1, opts, depth + 1)
                        .map_err(|e| e.at(&format!(".{}", f.name)))?
                }
                None => {
                    if let Some(d) = &f.default {
                        d.clone()
                    } else if f.ty.nullable {
                        DynValue::Null
                    } else if opts.missing_fields == MissingFields::Error {
                        return Err(DynError::invalid(format!(
                            "missing required field {}",
                            f.name
                        )));
                    } else {
                        self.zero_value(&f.ty, depth + 1)
                            .map_err(|e| e.at(&format!(".{}", f.name)))?
                    }
                }
            };
            out.push((f.name.clone(), value));
        }
        if opts.unknown_fields == UnknownFields::Error {
            if let Some(i) = used.iter().position(|u| !u) {
                if input.iter().filter(|(n, _)| *n == input[i].0).count() > 1 {
                    return Err(DynError::invalid(format!(
                        "field {} given twice",
                        input[i].0
                    )));
                }
                return Err(DynError::invalid(format!(
                    "unknown field {:?} for {} (fields: {})",
                    input[i].0,
                    t.full_name,
                    fields
                        .iter()
                        .map(|f| f.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        Ok(DynValue::Struct(DynStruct {
            type_name: t.full_name.clone(),
            fields: out,
        }))
    }

    fn coerce_enum(&self, idx: usize, v: &DynValue) -> DynResult<DynValue> {
        let t = &self.types[idx];
        let Body::Enum { values } = &t.body else {
            unreachable!()
        };
        let by_name = |name: &str| {
            values
                .iter()
                .find(|(n, _)| n == name)
                .or_else(|| values.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)))
        };
        let by_ord = |ord: u32| values.iter().find(|(_, o)| *o == ord);
        let found = match v {
            DynValue::Enum(e) => {
                if !e.type_name.is_empty() && self.index_of(&e.type_name) != Some(idx) {
                    return Err(DynError::invalid(format!(
                        "expected {}, got a {} enum",
                        t.full_name, e.type_name
                    )));
                }
                match &e.name {
                    Some(n) => by_name(n),
                    None => by_ord(e.ordinal),
                }
            }
            DynValue::String(s) => by_name(s).or_else(|| s.parse::<u32>().ok().and_then(by_ord)),
            other => match int_of(other) {
                Some(i) => u32::try_from(i).ok().and_then(by_ord),
                None => {
                    return Err(DynError::invalid(format!(
                        "expected a {} enum name or ordinal, got {}",
                        t.full_name,
                        other.kind_name()
                    )))
                }
            },
        };
        let (name, ordinal) = found.ok_or_else(|| {
            DynError::invalid(format!(
                "not a {} value (one of: {})",
                t.full_name,
                values
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok(DynValue::Enum(DynEnum {
            type_name: t.full_name.clone(),
            name: Some(name.clone()),
            ordinal: *ordinal,
        }))
    }

    fn coerce_union(
        &self,
        idx: usize,
        v: &DynValue,
        opts: &EncodeOptions,
        depth: u32,
    ) -> DynResult<DynValue> {
        let t = &self.types[idx];
        let Body::Union { variants } = &t.body else {
            unreachable!()
        };
        let variant_named = |tag: &str| {
            variants
                .iter()
                .find(|vr| vr.name == tag)
                .or_else(|| {
                    variants.iter().find(|vr| match &vr.payload.kind {
                        Kind::Struct(i) | Kind::Enum(i) | Kind::Union(i) => {
                            self.index_of(tag) == Some(*i)
                        }
                        _ => false,
                    })
                })
                .or_else(|| variants.iter().find(|vr| vr.name.eq_ignore_ascii_case(tag)))
        };
        let (variant, payload): (_, DynValue) = match v {
            DynValue::Union(u) => {
                if !u.type_name.is_empty() && self.index_of(&u.type_name) != Some(idx) {
                    return Err(DynError::invalid(format!(
                        "expected {}, got a {} union",
                        t.full_name, u.type_name
                    )));
                }
                let vr = match &u.case_name {
                    Some(n) => variant_named(n),
                    None => variants.iter().find(|vr| vr.case_id == u.case_id),
                };
                (vr, (*u.value).clone())
            }
            DynValue::Map(_) | DynValue::Struct(_) => {
                let entries: Vec<(String, DynValue)> = match v {
                    DynValue::Struct(s) => s.fields.clone(),
                    DynValue::Map(m) => {
                        let mut out = Vec::new();
                        for (k, val) in m {
                            let DynValue::String(k) = k else {
                                return Err(DynError::invalid("union object keys must be strings"));
                            };
                            out.push((k.clone(), val.clone()));
                        }
                        out
                    }
                    _ => unreachable!(),
                };
                if let Some(pos) = entries.iter().position(|(k, _)| k == "_type") {
                    let DynValue::String(tag) = &entries[pos].1 else {
                        return Err(DynError::invalid("union _type must be a string"));
                    };
                    let vr = variant_named(tag);
                    let rest: Vec<(String, DynValue)> = entries
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != pos)
                        .map(|(_, e)| e.clone())
                        .collect();
                    let payload = match vr.map(|vr| &vr.payload.kind) {
                        Some(Kind::Struct(_)) | None => DynValue::Map(
                            rest.into_iter()
                                .map(|(k, v)| (DynValue::String(k), v))
                                .collect(),
                        ),
                        Some(_) => rest
                            .into_iter()
                            .find(|(k, _)| k == "value")
                            .map(|(_, v)| v)
                            .unwrap_or(DynValue::Null),
                    };
                    if vr.is_none() {
                        return Err(DynError::invalid(format!(
                            "{tag:?} is not a {} case (one of: {})",
                            t.full_name,
                            variants
                                .iter()
                                .map(|v| v.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )));
                    }
                    (vr, payload)
                } else if entries.len() == 1 {
                    (variant_named(&entries[0].0), entries[0].1.clone())
                } else {
                    return Err(DynError::invalid(format!(
                        "a {} union needs {{\"_type\": <case>, ...}} or {{\"<case>\": value}}",
                        t.full_name
                    )));
                }
            }
            other => {
                return Err(DynError::invalid(format!(
                    "expected a {} union, got {}",
                    t.full_name,
                    other.kind_name()
                )))
            }
        };
        let vr = variant.ok_or_else(|| {
            DynError::invalid(format!(
                "no such {} case (one of: {})",
                t.full_name,
                variants
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        let value = self
            .coerce_ty(&vr.payload, &payload, opts, depth + 1)
            .map_err(|e| e.at(&format!("<{}>", vr.name)))?;
        Ok(DynValue::Union(DynUnion {
            type_name: t.full_name.clone(),
            case_id: vr.case_id,
            case_name: Some(vr.name.clone()),
            value: Box::new(value),
        }))
    }

    /// The value a typed Fory reader produces for an absent field: `null` for
    /// optional fields, the declared default, else the type's zero value.
    pub(crate) fn zero_value(&self, ty: &FieldTy, depth: u32) -> DynResult<DynValue> {
        if ty.nullable {
            return Ok(DynValue::Null);
        }
        if depth > MAX_DEPTH {
            return Err(DynError::invalid(
                "zero value of a recursive type without an optional break",
            ));
        }
        Ok(match &ty.kind {
            Kind::Prim(p) => match p {
                Prim::Bool => DynValue::Bool(false),
                Prim::Int8 => DynValue::I8(0),
                Prim::Int16 => DynValue::I16(0),
                Prim::Int32 | Prim::VarInt32 => DynValue::I32(0),
                Prim::Int64 | Prim::VarInt64 | Prim::TaggedInt64 => DynValue::I64(0),
                Prim::UInt8 => DynValue::U8(0),
                Prim::UInt16 => DynValue::U16(0),
                Prim::UInt32 | Prim::VarUInt32 => DynValue::U32(0),
                Prim::UInt64 | Prim::VarUInt64 | Prim::TaggedUInt64 => DynValue::U64(0),
                Prim::Float16 | Prim::BFloat16 | Prim::Float32 => DynValue::F32(0.0),
                Prim::Float64 => DynValue::F64(0.0),
                Prim::String => DynValue::String(String::new()),
                Prim::Binary => DynValue::Bytes(Vec::new()),
                Prim::Timestamp => DynValue::Timestamp {
                    seconds: 0,
                    nanos: 0,
                },
                Prim::Date => DynValue::Date { days: 0 },
                Prim::Duration => DynValue::Duration {
                    seconds: 0,
                    nanos: 0,
                },
                Prim::Decimal => DynValue::Decimal(DynDecimal::new(0, 0)),
                Prim::DecimalFixed { scale, .. } => {
                    DynValue::Decimal(DynDecimal::new(0, *scale as i32))
                }
                Prim::Uuid => DynValue::Uuid([0; 16]),
                Prim::Ulid => DynValue::Ulid([0; 16]),
            },
            Kind::List(_) => DynValue::List(vec![]),
            Kind::Set(_) => DynValue::Set(vec![]),
            Kind::Map(..) => DynValue::Map(vec![]),
            Kind::Struct(idx) => {
                let t = &self.types[*idx];
                let Body::Message { fields, .. } = &t.body else {
                    unreachable!()
                };
                let mut out = Vec::with_capacity(fields.len());
                for f in fields {
                    let v = match &f.default {
                        Some(d) => d.clone(),
                        None => self.zero_value(&f.ty, depth + 1)?,
                    };
                    out.push((f.name.clone(), v));
                }
                DynValue::Struct(DynStruct {
                    type_name: t.full_name.clone(),
                    fields: out,
                })
            }
            Kind::Enum(idx) => {
                let t = &self.types[*idx];
                let Body::Enum { values } = &t.body else {
                    unreachable!()
                };
                let (name, ordinal) = values
                    .iter()
                    .find(|(_, o)| *o == 0)
                    .or_else(|| values.first())
                    .ok_or_else(|| DynError::invalid(format!("{} has no values", t.full_name)))?;
                DynValue::Enum(DynEnum {
                    type_name: t.full_name.clone(),
                    name: Some(name.clone()),
                    ordinal: *ordinal,
                })
            }
            Kind::Union(idx) => {
                let t: &SType = &self.types[*idx];
                let Body::Union { variants } = &t.body else {
                    unreachable!()
                };
                let vr = variants
                    .iter()
                    .min_by_key(|v| v.case_id)
                    .ok_or_else(|| DynError::invalid(format!("{} has no cases", t.full_name)))?;
                DynValue::Union(DynUnion {
                    type_name: t.full_name.clone(),
                    case_id: vr.case_id,
                    case_name: Some(vr.name.clone()),
                    value: Box::new(self.zero_value(&vr.payload, depth + 1)?),
                })
            }
        })
    }
}

fn key_label(k: &DynValue, i: usize) -> String {
    match k {
        DynValue::String(s) => format!("{s:?}"),
        other => int_of(other)
            .map(|v| v.to_string())
            .unwrap_or_else(|| format!("#{i}")),
    }
}

/// An integer value of any width, as i128.
fn int_of(v: &DynValue) -> Option<i128> {
    Some(match v {
        DynValue::I8(x) => *x as i128,
        DynValue::I16(x) => *x as i128,
        DynValue::I32(x) => *x as i128,
        DynValue::I64(x) => *x as i128,
        DynValue::U8(x) => *x as i128,
        DynValue::U16(x) => *x as i128,
        DynValue::U32(x) => *x as i128,
        DynValue::U64(x) => *x as i128,
        DynValue::F64(f) if f.fract() == 0.0 && f.is_finite() && f.abs() < 1.9e19 => *f as i128,
        DynValue::F32(f) if f.fract() == 0.0 && f.is_finite() && f.abs() < 1.9e19 => *f as i128,
        _ => return None,
    })
}

fn coerce_int(p: Prim, v: &DynValue) -> DynResult<DynValue> {
    let i = match v {
        DynValue::String(s) => s
            .trim()
            .parse::<i128>()
            .map_err(|_| DynError::invalid(format!("{s:?} is not an integer")))?,
        other => int_of(other).ok_or_else(|| {
            DynError::invalid(format!(
                "expected an integer ({}), got {}",
                p.contract_name(),
                other.kind_name()
            ))
        })?,
    };
    let range = || DynError::invalid(format!("{i} is out of range for {}", p.contract_name()));
    Ok(match p {
        Prim::Int8 => DynValue::I8(i8::try_from(i).map_err(|_| range())?),
        Prim::Int16 => DynValue::I16(i16::try_from(i).map_err(|_| range())?),
        Prim::Int32 | Prim::VarInt32 => DynValue::I32(i32::try_from(i).map_err(|_| range())?),
        Prim::Int64 | Prim::VarInt64 | Prim::TaggedInt64 => {
            DynValue::I64(i64::try_from(i).map_err(|_| range())?)
        }
        Prim::UInt8 => DynValue::U8(u8::try_from(i).map_err(|_| range())?),
        Prim::UInt16 => DynValue::U16(u16::try_from(i).map_err(|_| range())?),
        Prim::UInt32 | Prim::VarUInt32 => DynValue::U32(u32::try_from(i).map_err(|_| range())?),
        Prim::UInt64 | Prim::VarUInt64 | Prim::TaggedUInt64 => {
            DynValue::U64(u64::try_from(i).map_err(|_| range())?)
        }
        _ => unreachable!(),
    })
}

fn float_of(v: &DynValue) -> DynResult<f64> {
    match v {
        DynValue::F64(f) => Ok(*f),
        DynValue::F32(f) => Ok(*f as f64),
        DynValue::String(s) => match s.trim() {
            "NaN" | "nan" => Ok(f64::NAN),
            "Infinity" | "inf" | "+Infinity" => Ok(f64::INFINITY),
            "-Infinity" | "-inf" => Ok(f64::NEG_INFINITY),
            t => t
                .parse::<f64>()
                .map_err(|_| DynError::invalid(format!("{s:?} is not a number"))),
        },
        other => int_of(other).map(|i| i as f64).ok_or_else(|| {
            DynError::invalid(format!("expected a number, got {}", other.kind_name()))
        }),
    }
}

fn coerce_prim(p: Prim, v: &DynValue) -> DynResult<DynValue> {
    match p {
        Prim::Bool => match v {
            DynValue::Bool(b) => Ok(DynValue::Bool(*b)),
            DynValue::String(s) if s == "true" => Ok(DynValue::Bool(true)),
            DynValue::String(s) if s == "false" => Ok(DynValue::Bool(false)),
            other => Err(DynError::invalid(format!(
                "expected a bool, got {}",
                other.kind_name()
            ))),
        },
        Prim::Int8
        | Prim::Int16
        | Prim::Int32
        | Prim::VarInt32
        | Prim::Int64
        | Prim::VarInt64
        | Prim::TaggedInt64
        | Prim::UInt8
        | Prim::UInt16
        | Prim::UInt32
        | Prim::VarUInt32
        | Prim::UInt64
        | Prim::VarUInt64
        | Prim::TaggedUInt64 => coerce_int(p, v),
        Prim::Float16 | Prim::BFloat16 | Prim::Float32 => Ok(DynValue::F32(float_of(v)? as f32)),
        Prim::Float64 => Ok(DynValue::F64(float_of(v)?)),
        Prim::String => match v {
            DynValue::String(s) => Ok(DynValue::String(s.clone())),
            other => Err(DynError::invalid(format!(
                "expected a string, got {}",
                other.kind_name()
            ))),
        },
        Prim::Binary => match v {
            DynValue::Bytes(b) => Ok(DynValue::Bytes(b.clone())),
            DynValue::String(s) => super::json::decode_base64(s)
                .map(DynValue::Bytes)
                .ok_or_else(|| DynError::invalid("binary given as a string must be base64")),
            DynValue::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match int_of(item).and_then(|i| u8::try_from(i).ok()) {
                        Some(b) => out.push(b),
                        None => return Err(DynError::invalid("binary list items must be 0..=255")),
                    }
                }
                Ok(DynValue::Bytes(out))
            }
            other => Err(DynError::invalid(format!(
                "expected bytes, got {}",
                other.kind_name()
            ))),
        },
        Prim::Timestamp => match v {
            DynValue::Timestamp { .. } => Ok(v.clone()),
            DynValue::String(s) => super::json::parse_timestamp(s)
                .map(|(seconds, nanos)| DynValue::Timestamp { seconds, nanos })
                .ok_or_else(|| DynError::invalid(format!("{s:?} is not an ISO-8601 timestamp"))),
            other => Err(DynError::invalid(format!(
                "expected a timestamp, got {}",
                other.kind_name()
            ))),
        },
        Prim::Date => match v {
            DynValue::Date { .. } => Ok(v.clone()),
            DynValue::String(s) => super::json::parse_date(s)
                .map(|days| DynValue::Date { days })
                .ok_or_else(|| DynError::invalid(format!("{s:?} is not a YYYY-MM-DD date"))),
            other => match int_of(other).and_then(|i| i64::try_from(i).ok()) {
                Some(days) => Ok(DynValue::Date { days }),
                None => Err(DynError::invalid(format!(
                    "expected a date, got {}",
                    other.kind_name()
                ))),
            },
        },
        Prim::Duration => match v {
            DynValue::Duration { .. } => Ok(v.clone()),
            DynValue::Map(_) | DynValue::Struct(_) => {
                let get = |name: &str| -> Option<&DynValue> {
                    match v {
                        DynValue::Struct(s) => s.get(name),
                        DynValue::Map(m) => m
                            .iter()
                            .find(|(k, _)| matches!(k, DynValue::String(s) if s == name))
                            .map(|(_, v)| v),
                        _ => None,
                    }
                };
                let seconds = match get("seconds") {
                    Some(s) => match coerce_int(Prim::Int64, s)? {
                        DynValue::I64(x) => x,
                        _ => unreachable!(),
                    },
                    None => 0,
                };
                let nanos = match get("nanos") {
                    Some(n) => match coerce_int(Prim::Int32, n)? {
                        DynValue::I32(x) => x,
                        _ => unreachable!(),
                    },
                    None => 0,
                };
                Ok(DynValue::Duration { seconds, nanos })
            }
            other => {
                let secs = float_of(other)?;
                if !secs.is_finite() {
                    return Err(DynError::invalid("duration must be finite"));
                }
                let whole = secs.trunc();
                Ok(DynValue::Duration {
                    seconds: whole as i64,
                    nanos: ((secs - whole) * 1e9).round() as i32,
                })
            }
        },
        Prim::Decimal => decimal_of(v).map(DynValue::Decimal),
        Prim::DecimalFixed { precision, scale } => decimal_of(v)?
            .rescale(precision, scale)
            .map(DynValue::Decimal)
            .map_err(DynError::invalid),
        Prim::Uuid => id_of(v, "uuid", scalars::parse_uuid).map(DynValue::Uuid),
        Prim::Ulid => id_of(v, "ulid", scalars::parse_ulid).map(DynValue::Ulid),
    }
}

/// A decimal from a decimal value, a decimal string (plain or exponent
/// notation), or an integer.
///
/// A float is refused, as Python and TypeScript refuse it: a JSON number with
/// a fraction or past `u64` has already been rounded to an `f64` by the time
/// it gets here, so its digits may not be the ones the caller sent, and the
/// spec forbids rounding a decimal (Aster-ContractIdentity.md, `decimal(p,s)`).
/// Send decimals as strings.
fn decimal_of(v: &DynValue) -> DynResult<DynDecimal> {
    let parse = |s: &str| {
        DynDecimal::parse(s).ok_or_else(|| DynError::invalid(format!("{s:?} is not a decimal")))
    };
    match v {
        DynValue::Decimal(d) => Ok(d.clone()),
        DynValue::String(s) => parse(s),
        DynValue::F64(_) | DynValue::F32(_) => Err(DynError::invalid(
            "a decimal must be a string or an integer, not a float: a float may \
             already have lost digits (send \"12.34\", not 12.34)",
        )),
        other => match int_of(other) {
            Some(i) => Ok(DynDecimal::new(i, 0)),
            None => Err(DynError::invalid(format!(
                "expected a decimal, got {}",
                other.kind_name()
            ))),
        },
    }
}

/// A 16-byte identifier from its string form, 16 bytes, or the same
/// identifier type. A `uuid` is not accepted for a `ulid` or the reverse:
/// they are distinct contract types.
fn id_of(v: &DynValue, what: &str, parse: fn(&str) -> Option<[u8; 16]>) -> DynResult<[u8; 16]> {
    match (v, what) {
        (DynValue::Uuid(b), "uuid") | (DynValue::Ulid(b), "ulid") => Ok(*b),
        (DynValue::String(s), _) => {
            parse(s).ok_or_else(|| DynError::invalid(format!("{s:?} is not a {what} string")))
        }
        (DynValue::Bytes(b), _) => scalars::id16(b, what).map_err(DynError::invalid),
        (other, _) => Err(DynError::invalid(format!(
            "expected a {what}, got {}",
            other.kind_name()
        ))),
    }
}
