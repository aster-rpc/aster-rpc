//! Schema-driven encoder: canonical [`DynValue`] → Fory 1.7.6 xlang bytes
//! (`xlang=true, compatible=true, ref=false`).

use std::collections::HashMap;

use fory_core::buffer::Writer;
use fory_core::type_id as tid;
use fory_core::{BFloat16, Float16, RefFlag};

use super::schema::{Body, DynSchema, FieldTy, Kind, Prim};
use super::value::{DynError, DynResult, DynValue};
use super::{EncodeOptions, StringEncoding};

// Collection element header bits (fory-core `serializer::collection`).
const COLL_HAS_NULL: u8 = 0b10;
const COLL_DECL_ELEMENT_TYPE: u8 = 0b100;
const COLL_IS_SAME_TYPE: u8 = 0b1000;
// Map chunk header bits (fory-core `serializer::map`).
const MAP_KEY_NULL: u8 = 0b10;
const MAP_DECL_KEY_TYPE: u8 = 0b100;
const MAP_VALUE_NULL: u8 = 0b10000;
const MAP_DECL_VALUE_TYPE: u8 = 0b100000;
const MAP_MAX_CHUNK: u8 = 255;
/// `IS_CROSS_LANGUAGE_FLAG` in the head bitmap.
pub(crate) const XLANG_FLAG: u8 = 1;

pub(crate) struct Encoder<'s, 'w> {
    schema: &'s DynSchema,
    w: Writer<'w>,
    /// Streamed TypeDef index per schema type, in first-write order.
    metas: HashMap<usize, u32>,
    strings: StringEncoding,
}

/// Whether Fory writes a value's type info before it in a field, list or map
/// slot (`need_to_write_type_for_field`): structs do, everything else is
/// declared by the schema.
fn writes_type_info(k: &Kind) -> bool {
    matches!(k, Kind::Struct(_))
}

impl<'s, 'w> Encoder<'s, 'w> {
    pub(crate) fn new(schema: &'s DynSchema, buf: &'w mut Vec<u8>, opts: &EncodeOptions) -> Self {
        Encoder {
            schema,
            w: Writer::from_buffer(buf),
            metas: HashMap::new(),
            strings: opts.string_encoding,
        }
    }

    /// A whole payload: head bitmap, root ref flag, type info, value.
    pub(crate) fn write_root(&mut self, idx: usize, v: &DynValue) -> DynResult<()> {
        self.w.write_u8(XLANG_FLAG);
        self.w.write_i8(RefFlag::NotNullValue as i8);
        self.write_type_info(idx)?;
        self.write_user_data(idx, v)
    }

    fn write_type_info(&mut self, idx: usize) -> DynResult<()> {
        let t = &self.schema.types[idx];
        self.w.write_u8(t.wire_type_id() as u8);
        match self.metas.get(&idx) {
            Some(&i) => self.w.write_var_u32((i << 1) | 1),
            None => {
                let i = self.metas.len() as u32;
                self.metas.insert(idx, i);
                self.w.write_var_u32(i << 1);
                self.w.write_bytes(&t.type_meta);
            }
        }
        Ok(())
    }

    /// The data of a struct / enum / union value (no type info).
    fn write_user_data(&mut self, idx: usize, v: &DynValue) -> DynResult<()> {
        let schema = self.schema;
        let t = &schema.types[idx];
        match (&t.body, v) {
            (Body::Message { fields, wire_order }, DynValue::Struct(s)) => {
                if s.fields.len() != fields.len() {
                    return Err(DynError::invalid(
                        "struct is not canonical (coerce it first)",
                    ));
                }
                for &fi in wire_order {
                    let f = &fields[fi];
                    let value = &s.fields[fi];
                    if value.0 != f.name {
                        return Err(DynError::invalid(
                            "struct is not canonical (coerce it first)",
                        ));
                    }
                    self.write_field(&f.ty, &value.1)
                        .map_err(|e| e.at(&format!(".{}", f.name)))?;
                }
                Ok(())
            }
            (Body::Enum { .. }, DynValue::Enum(e)) => {
                self.w.write_var_u32(e.ordinal);
                Ok(())
            }
            (Body::Union { variants }, DynValue::Union(u)) => {
                let vr = variants
                    .iter()
                    .find(|vr| vr.case_id == u.case_id)
                    .ok_or_else(|| DynError::invalid(format!("no case id {}", u.case_id)))?;
                self.w.write_var_u32(u.case_id);
                // The case payload is written like a ref-tracked "any" slot:
                // ref flag, type info, data (fory-derive `gen_write_single_payload`).
                if u.value.is_null() {
                    self.w.write_i8(RefFlag::Null as i8);
                    return Ok(());
                }
                self.w.write_i8(RefFlag::RefValue as i8);
                self.write_any_type_info(&vr.payload.kind)?;
                self.write_data(&vr.payload.kind, &u.value)
                    .map_err(|e| e.at(&format!("<{}>", vr.name)))
            }
            (_, other) => Err(DynError::invalid(format!(
                "expected a {} value, got {}",
                t.full_name,
                other.kind_name()
            ))),
        }
    }

    /// Type info for a polymorphic slot (union payload).
    fn write_any_type_info(&mut self, k: &Kind) -> DynResult<()> {
        match k {
            Kind::Struct(i) | Kind::Enum(i) | Kind::Union(i) => self.write_type_info(*i),
            Kind::Prim(p) => {
                self.w.write_u8(p.type_id() as u8);
                Ok(())
            }
            Kind::List(_) => {
                self.w.write_u8(tid::LIST as u8);
                Ok(())
            }
            Kind::Set(_) => {
                self.w.write_u8(tid::SET as u8);
                Ok(())
            }
            Kind::Map(..) => {
                self.w.write_u8(tid::MAP as u8);
                Ok(())
            }
        }
    }

    /// A struct field: null flag when nullable, type info for structs, data.
    fn write_field(&mut self, ty: &FieldTy, v: &DynValue) -> DynResult<()> {
        if ty.nullable {
            if v.is_null() {
                self.w.write_i8(RefFlag::Null as i8);
                return Ok(());
            }
            self.w.write_i8(RefFlag::NotNullValue as i8);
        } else if v.is_null() {
            return Err(DynError::invalid("null for a non-optional field"));
        }
        if let Kind::Struct(i) = ty.kind {
            self.write_type_info(i)?;
        }
        self.write_data(&ty.kind, v)
    }

    fn write_data(&mut self, k: &Kind, v: &DynValue) -> DynResult<()> {
        match k {
            Kind::Prim(p) => self.write_prim(*p, v),
            Kind::Struct(i) | Kind::Enum(i) | Kind::Union(i) => self.write_user_data(*i, v),
            Kind::List(e) | Kind::Set(e) => match v {
                DynValue::List(items) | DynValue::Set(items) => self.write_collection(e, items),
                other => Err(DynError::invalid(format!(
                    "expected a list, got {}",
                    other.kind_name()
                ))),
            },
            Kind::Map(kt, vt) => match v {
                DynValue::Map(entries) => self.write_map(kt, vt, entries),
                other => Err(DynError::invalid(format!(
                    "expected a map, got {}",
                    other.kind_name()
                ))),
            },
        }
    }

    fn write_collection(&mut self, e: &FieldTy, items: &[DynValue]) -> DynResult<()> {
        self.w.write_var_u32(len_u32(items.len())?);
        if items.is_empty() {
            return Ok(());
        }
        let has_null = items.iter().any(DynValue::is_null);
        if has_null && !e.nullable {
            return Err(DynError::invalid(
                "null element in a list of non-optional values",
            ));
        }
        let mut header = COLL_IS_SAME_TYPE;
        if has_null {
            header |= COLL_HAS_NULL;
        }
        let declared = !writes_type_info(&e.kind);
        if declared {
            header |= COLL_DECL_ELEMENT_TYPE;
        }
        self.w.write_u8(header);
        if let (false, Kind::Struct(i)) = (declared, &e.kind) {
            self.write_type_info(*i)?;
        }
        for (i, item) in items.iter().enumerate() {
            if has_null {
                if item.is_null() {
                    self.w.write_i8(RefFlag::Null as i8);
                    continue;
                }
                self.w.write_i8(RefFlag::NotNullValue as i8);
            }
            self.write_data(&e.kind, item)
                .map_err(|err| err.at(&format!("[{i}]")))?;
        }
        Ok(())
    }

    fn write_map(
        &mut self,
        kt: &FieldTy,
        vt: &FieldTy,
        entries: &[(DynValue, DynValue)],
    ) -> DynResult<()> {
        self.w.write_var_u32(len_u32(entries.len())?);
        let key_declared = !writes_type_info(&kt.kind);
        let value_declared = !writes_type_info(&vt.kind);
        let mut chunk_at: Option<usize> = None;
        let mut count: u8 = 0;
        for (i, (k, v)) in entries.iter().enumerate() {
            let at = |e: DynError| e.at(&format!("{{entry {i}}}"));
            if (k.is_null() && !kt.nullable) || (v.is_null() && !vt.nullable) {
                return Err(at(DynError::invalid(
                    "null in a map slot that is not optional",
                )));
            }
            if k.is_null() || v.is_null() {
                if let Some(pos) = chunk_at.take() {
                    self.w.set_bytes(pos + 1, &[count]);
                    count = 0;
                }
                if k.is_null() && v.is_null() {
                    self.w.write_u8(MAP_KEY_NULL | MAP_VALUE_NULL);
                } else if v.is_null() {
                    self.w.write_u8(
                        MAP_VALUE_NULL | if key_declared { MAP_DECL_KEY_TYPE } else { 0 },
                    );
                    self.write_slot(kt, key_declared, k).map_err(at)?;
                } else {
                    self.w.write_u8(
                        MAP_KEY_NULL
                            | if value_declared {
                                MAP_DECL_VALUE_TYPE
                            } else {
                                0
                            },
                    );
                    self.write_slot(vt, value_declared, v).map_err(at)?;
                }
                continue;
            }
            if chunk_at.is_none() {
                let pos = self.w.len();
                let mut header = 0u8;
                if key_declared {
                    header |= MAP_DECL_KEY_TYPE;
                }
                if value_declared {
                    header |= MAP_DECL_VALUE_TYPE;
                }
                self.w.write_u8(header);
                self.w.write_u8(0); // pair count, patched below
                if let Kind::Struct(idx) = kt.kind {
                    self.write_type_info(idx)?;
                }
                if let Kind::Struct(idx) = vt.kind {
                    self.write_type_info(idx)?;
                }
                chunk_at = Some(pos);
            }
            self.write_data(&kt.kind, k).map_err(at)?;
            self.write_data(&vt.kind, v).map_err(at)?;
            count += 1;
            if count == MAP_MAX_CHUNK {
                let pos = chunk_at.take().unwrap();
                self.w.set_bytes(pos + 1, &[count]);
                count = 0;
            }
        }
        if let Some(pos) = chunk_at {
            self.w.set_bytes(pos + 1, &[count]);
        }
        Ok(())
    }

    /// One side of a null-partnered map entry: type info unless declared, data.
    fn write_slot(&mut self, ty: &FieldTy, declared: bool, v: &DynValue) -> DynResult<()> {
        if !declared {
            if let Kind::Struct(i) = ty.kind {
                self.write_type_info(i)?;
            }
        }
        self.write_data(&ty.kind, v)
    }

    fn write_string(&mut self, s: &str) -> DynResult<()> {
        let (encoding, bytes): (u64, std::borrow::Cow<[u8]>) = match self.strings {
            StringEncoding::Utf8 => (2, s.as_bytes().into()),
            StringEncoding::Latin1OrUtf16 | StringEncoding::Pyfory => {
                let max = s.chars().map(|c| c as u32).max().unwrap_or(0);
                if max <= 0xFF {
                    (0, s.chars().map(|c| c as u8).collect::<Vec<u8>>().into())
                } else if max <= 0xFFFF || self.strings == StringEncoding::Latin1OrUtf16 {
                    (
                        1,
                        s.encode_utf16()
                            .flat_map(|u| u.to_le_bytes())
                            .collect::<Vec<u8>>()
                            .into(),
                    )
                } else {
                    (2, s.as_bytes().into())
                }
            }
        };
        if bytes.len() as u64 >= 1u64 << 34 {
            return Err(DynError::invalid("string too long"));
        }
        self.w
            .write_var_u36_small(((bytes.len() as u64) << 2) | encoding);
        self.w.write_bytes(&bytes);
        Ok(())
    }

    fn write_prim(&mut self, p: Prim, v: &DynValue) -> DynResult<()> {
        let w = &mut self.w;
        match (p, v) {
            (Prim::Bool, DynValue::Bool(b)) => w.write_bool(*b),
            (Prim::Int8, DynValue::I8(x)) => w.write_i8(*x),
            (Prim::Int16, DynValue::I16(x)) => w.write_i16(*x),
            (Prim::Int32, DynValue::I32(x)) => w.write_i32(*x),
            (Prim::VarInt32, DynValue::I32(x)) => w.write_var_i32(*x),
            (Prim::Int64, DynValue::I64(x)) => w.write_i64(*x),
            (Prim::VarInt64, DynValue::I64(x)) => w.write_var_i64(*x),
            (Prim::TaggedInt64, DynValue::I64(x)) => w.write_tagged_i64(*x),
            (Prim::UInt8, DynValue::U8(x)) => w.write_u8(*x),
            (Prim::UInt16, DynValue::U16(x)) => w.write_u16(*x),
            (Prim::UInt32, DynValue::U32(x)) => w.write_u32(*x),
            (Prim::VarUInt32, DynValue::U32(x)) => w.write_var_u32(*x),
            (Prim::UInt64, DynValue::U64(x)) => w.write_u64(*x),
            (Prim::VarUInt64, DynValue::U64(x)) => w.write_var_u64(*x),
            (Prim::TaggedUInt64, DynValue::U64(x)) => w.write_tagged_u64(*x),
            (Prim::Float16, DynValue::F32(x)) => w.write_f16(Float16::from_f32(*x)),
            (Prim::BFloat16, DynValue::F32(x)) => w.write_bf16(BFloat16::from_f32(*x)),
            (Prim::Float32, DynValue::F32(x)) => w.write_f32(*x),
            (Prim::Float64, DynValue::F64(x)) => w.write_f64(*x),
            (Prim::String, DynValue::String(s)) => return self.write_string(s),
            (Prim::Binary, DynValue::Bytes(b)) => {
                w.write_var_u32(len_u32(b.len())?);
                w.write_bytes(b);
            }
            (Prim::Timestamp, DynValue::Timestamp { seconds, nanos }) => {
                w.write_i64(*seconds);
                w.write_u32(*nanos);
            }
            (Prim::Date, DynValue::Date { days }) => w.write_var_i64(*days),
            (Prim::Duration, DynValue::Duration { seconds, nanos }) => {
                w.write_var_i64(*seconds);
                w.write_i32(*nanos);
            }
            (Prim::Decimal, DynValue::Decimal(d)) => return d.write(w),
            (Prim::DecimalFixed { precision, scale }, DynValue::Decimal(d)) => {
                let d = d.rescale(precision, scale).map_err(DynError::invalid)?;
                return d.write(w);
            }
            (Prim::Uuid, DynValue::Uuid(b)) | (Prim::Ulid, DynValue::Ulid(b)) => {
                w.write_var_u32(16);
                w.write_bytes(b);
            }
            (p, other) => {
                return Err(DynError::invalid(format!(
                    "expected {}, got {} (coerce the value first)",
                    p.contract_name(),
                    other.kind_name()
                )))
            }
        }
        Ok(())
    }
}

fn len_u32(n: usize) -> DynResult<u32> {
    u32::try_from(n).map_err(|_| DynError::invalid("collection too large"))
}
