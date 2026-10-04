//! Wire-driven decoder: Fory 1.7.6 xlang compatible bytes → [`DynValue`].
//!
//! Compatible mode streams every struct's TypeDef (field names and types) in
//! the payload, so the decoder follows the *sender's* layout: field order,
//! fields the local schema lacks, and integer widths all come from the wire.
//! The local [`DynSchema`] only supplies names the wire omits (enum value and
//! union case names) and the local view of a struct (declared field names,
//! defaults for fields the sender lacks).

use std::rc::Rc;

use fory_core::buffer::Reader;
use fory_core::meta::{FieldInfo, FieldType, TypeMeta};
use fory_core::type_id as tid;
use fory_core::util::murmurhash3_x64_128;

use super::coerce::MAX_DEPTH;
use super::encode::XLANG_FLAG;
use super::schema::{Body, DynSchema, FieldTy, Kind, Prim};
use super::value::{DynEnum, DynError, DynResult, DynStruct, DynUnion, DynValue};
use super::DecodeOptions;

const REF_FLAG_NULL: i8 = -3;
const REF_FLAG_REF: i8 = -2;
const OUT_OF_BAND_FLAG: u8 = 2;

const COLL_TRACKING_REF: u8 = 0b1;
const COLL_HAS_NULL: u8 = 0b10;
const COLL_DECL_ELEMENT_TYPE: u8 = 0b100;
const COLL_IS_SAME_TYPE: u8 = 0b1000;

const MAP_TRACKING_KEY_REF: u8 = 0b1;
const MAP_KEY_NULL: u8 = 0b10;
const MAP_DECL_KEY_TYPE: u8 = 0b100;
const MAP_TRACKING_VALUE_REF: u8 = 0b1000;
const MAP_VALUE_NULL: u8 = 0b10000;
const MAP_DECL_VALUE_TYPE: u8 = 0b100000;

// TypeMeta layout (fory-core `meta::type_meta`).
const META_SIZE_MASK: i64 = 0xff;
const COMPRESS_META_FLAG: i64 = 1 << 8;
const RESERVED_META_FLAGS: i64 = 0b111 << 9;
const META_HASH_SHIFT: u32 = 12;
const STRUCT_TYPEDEF_FLAG: u8 = 0b1000_0000;
const REGISTER_BY_NAME_FLAG: u8 = 0b0010_0000;
const COMPATIBLE_TYPEDEF_FLAG: u8 = 0b0100_0000;
const SMALL_NUM_FIELDS: usize = 0b11111;
const MAX_META_FIELDS: usize = 512;
const MAX_META_BYTES: usize = 64 * 1024;

/// A TypeDef read from the stream.
#[derive(Debug)]
pub(crate) struct WireMeta {
    namespace: String,
    name: String,
    fields: Vec<(String, FieldType)>,
    /// The matching local schema type, by `(namespace, name)`.
    local: Option<usize>,
}

impl WireMeta {
    fn full_name(&self) -> String {
        if self.namespace.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.namespace, self.name)
        }
    }
}

/// Type info read before a value.
struct TypeInfo {
    type_id: u32,
    meta: Option<Rc<WireMeta>>,
}

pub(crate) struct Decoder<'s, 'b> {
    schema: &'s DynSchema,
    r: Reader<'b>,
    metas: Vec<Rc<WireMeta>>,
    opts: DecodeOptions,
    depth: u32,
}

impl<'s, 'b> Decoder<'s, 'b> {
    pub(crate) fn new(schema: &'s DynSchema, bytes: &'b [u8], opts: &DecodeOptions) -> Self {
        Decoder {
            schema,
            r: Reader::new(bytes),
            metas: Vec::new(),
            opts: opts.clone(),
            depth: 0,
        }
    }

    /// A whole payload. `expect` (a schema index) checks the root type.
    pub(crate) fn read_root(&mut self, expect: Option<usize>) -> DynResult<DynValue> {
        let bitmap = self.r.read_u8()?;
        if bitmap & XLANG_FLAG == 0 {
            return Err(DynError::decode("not a Fory xlang payload"));
        }
        if bitmap & OUT_OF_BAND_FLAG != 0 {
            return Err(DynError::unsupported("out-of-band Fory buffers"));
        }
        let flag = self.r.read_i8()?;
        let value = match flag {
            REF_FLAG_NULL => DynValue::Null,
            REF_FLAG_REF => return Err(DynError::decode("root is a back-reference")),
            _ => {
                let info = self.read_type_info()?;
                if let Some(want) = expect {
                    let got = info.meta.as_ref().and_then(|m| m.local);
                    if got != Some(want) {
                        let wire = info
                            .meta
                            .as_ref()
                            .map(|m| m.full_name())
                            .unwrap_or_else(|| format!("type id {}", info.type_id));
                        return Err(DynError::decode(format!(
                            "payload is a {wire}, expected {}",
                            self.schema.types[want].full_name
                        )));
                    }
                }
                let hint = expect.map(|i| FieldTy {
                    kind: self.schema.kind_of(i),
                    nullable: false,
                });
                self.read_with_info(&info, hint.as_ref())?
            }
        };
        if !self.r.slice_after_cursor().is_empty() {
            return Err(DynError::decode(format!(
                "{} trailing bytes after the payload",
                self.r.slice_after_cursor().len()
            )));
        }
        Ok(value)
    }

    fn enter(&mut self) -> DynResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(DynError::decode("payload nests too deeply"));
        }
        Ok(())
    }

    fn read_type_info(&mut self) -> DynResult<TypeInfo> {
        let type_id = self.r.read_u8()? as u32;
        let meta = match type_id {
            tid::COMPATIBLE_STRUCT
            | tid::NAMED_COMPATIBLE_STRUCT
            | tid::NAMED_STRUCT
            | tid::NAMED_ENUM
            | tid::NAMED_EXT
            | tid::NAMED_UNION => Some(self.read_meta()?),
            tid::ENUM | tid::STRUCT | tid::EXT | tid::TYPED_UNION => {
                // Registered by numeric id; Aster registers by name, so there
                // is nothing to resolve the id against.
                let _user_type_id = self.r.read_var_u32()?;
                None
            }
            _ => None,
        };
        Ok(TypeInfo { type_id, meta })
    }

    fn read_meta(&mut self) -> DynResult<Rc<WireMeta>> {
        let marker = self.r.read_var_u32()?;
        let index = (marker >> 1) as usize;
        if marker & 1 == 1 {
            return self.metas.get(index).cloned().ok_or_else(|| {
                DynError::decode(format!("TypeDef reference {index} not yet defined"))
            });
        }
        if index != self.metas.len() {
            return Err(DynError::decode("TypeDef index out of sequence"));
        }
        let header = self.r.read_i64()?;
        if header & (RESERVED_META_FLAGS | COMPRESS_META_FLAG) != 0 {
            return Err(DynError::unsupported(
                "compressed or flagged TypeDef header",
            ));
        }
        let mut size = (header & META_SIZE_MASK) as usize;
        if size == META_SIZE_MASK as usize {
            size += self.r.read_var_u32()? as usize;
        }
        if size > MAX_META_BYTES {
            return Err(DynError::decode(format!("TypeDef of {size} bytes")));
        }
        let body = self.r.read_bytes(size)?.to_vec();
        check_meta_hash(header, &body)?;
        let meta = Rc::new(self.parse_meta(&body)?);
        self.metas.push(meta.clone());
        Ok(meta)
    }

    fn parse_meta(&self, body: &[u8]) -> DynResult<WireMeta> {
        let mut r = Reader::new(body);
        let h = r.read_u8()?;
        let (_type_id, by_name, mut num_fields) = if h & STRUCT_TYPEDEF_FLAG != 0 {
            let by_name = h & REGISTER_BY_NAME_FLAG != 0;
            let compatible = h & COMPATIBLE_TYPEDEF_FLAG != 0;
            let id = match (by_name, compatible) {
                (true, true) => tid::NAMED_COMPATIBLE_STRUCT,
                (true, false) => tid::NAMED_STRUCT,
                (false, true) => tid::COMPATIBLE_STRUCT,
                (false, false) => tid::STRUCT,
            };
            (id, by_name, (h as usize) & SMALL_NUM_FIELDS)
        } else {
            let id = match h & 0x0f {
                0 => tid::ENUM,
                1 => tid::NAMED_ENUM,
                2 => tid::EXT,
                3 => tid::NAMED_EXT,
                4 => tid::TYPED_UNION,
                5 => tid::NAMED_UNION,
                k => return Err(DynError::decode(format!("TypeDef kind {k}"))),
            };
            let by_name = matches!(id, tid::NAMED_ENUM | tid::NAMED_EXT | tid::NAMED_UNION);
            (id, by_name, 0)
        };
        if h & STRUCT_TYPEDEF_FLAG != 0 && num_fields == SMALL_NUM_FIELDS {
            num_fields += r.read_var_u32()? as usize;
        }
        if num_fields > MAX_META_FIELDS {
            return Err(DynError::decode(format!(
                "TypeDef with {num_fields} fields"
            )));
        }
        let (namespace, name) = if by_name {
            let ns = TypeMeta::read_namespace(&mut r)?.original;
            let n = TypeMeta::read_type_name(&mut r)?.original;
            (ns, n)
        } else {
            let id = r.read_var_u32()?;
            (String::new(), format!("#{id}"))
        };
        let mut fields = Vec::with_capacity(num_fields);
        for _ in 0..num_fields {
            let fi = FieldInfo::from_bytes(&mut r)?;
            let name = if fi.field_id >= 0 {
                format!("#{}", fi.field_id)
            } else {
                fi.field_name
            };
            fields.push((name, fi.field_type));
        }
        if !r.slice_after_cursor().is_empty() {
            return Err(DynError::decode("TypeDef body has trailing bytes"));
        }
        let local = if by_name {
            self.schema.index_by_fory(&namespace, &name)
        } else {
            None
        };
        Ok(WireMeta {
            namespace,
            name,
            fields,
            local,
        })
    }

    /// A value whose type info was just read.
    fn read_with_info(&mut self, info: &TypeInfo, hint: Option<&FieldTy>) -> DynResult<DynValue> {
        self.enter()?;
        let v = self.read_with_info_inner(info, hint);
        self.depth -= 1;
        v
    }

    fn read_with_info_inner(
        &mut self,
        info: &TypeInfo,
        hint: Option<&FieldTy>,
    ) -> DynResult<DynValue> {
        let id = info.type_id;
        if let Some(p) = Prim::from_type_id(id) {
            return self.read_prim(p);
        }
        match id {
            tid::LIST | tid::SET => self.read_collection(None, hint, id == tid::SET),
            tid::MAP => self.read_map(None, None, hint),
            tid::COMPATIBLE_STRUCT | tid::NAMED_COMPATIBLE_STRUCT | tid::NAMED_STRUCT => {
                let meta = info.meta.clone().expect("struct type info has a TypeDef");
                self.read_struct(&meta)
            }
            tid::NAMED_ENUM | tid::ENUM => {
                let local = info
                    .meta
                    .as_ref()
                    .and_then(|m| m.local)
                    .or_else(|| hint_index(hint, |k| matches!(k, Kind::Enum(_))));
                let wire_name = info.meta.as_ref().map(|m| m.full_name());
                self.read_enum(local, wire_name)
            }
            tid::NAMED_UNION | tid::TYPED_UNION | tid::UNION => {
                let local = info
                    .meta
                    .as_ref()
                    .and_then(|m| m.local)
                    .or_else(|| hint_index(hint, |k| matches!(k, Kind::Union(_))));
                let wire_name = info.meta.as_ref().map(|m| m.full_name());
                self.read_union(local, wire_name)
            }
            tid::STRUCT | tid::EXT | tid::NAMED_EXT => Err(DynError::unsupported(format!(
                "Fory type id {id} (id-registered struct or ext type) cannot be decoded without a typed registration"
            ))),
            _ => self.read_primitive_array(id),
        }
    }

    /// A struct field / declared slot: null flag when nullable (or ref
    /// tracked), then the data.
    fn read_field(&mut self, ft: &FieldType, hint: Option<&FieldTy>) -> DynResult<DynValue> {
        if ft.nullable || ft.track_ref {
            match self.r.read_i8()? {
                REF_FLAG_NULL => return Ok(DynValue::Null),
                REF_FLAG_REF => {
                    return Err(DynError::unsupported(
                        "shared references (Aster payloads run with ref tracking off)",
                    ))
                }
                _ => {}
            }
        }
        self.read_declared(ft, hint)
    }

    /// Data whose type the schema declares (no flag).
    fn read_declared(&mut self, ft: &FieldType, hint: Option<&FieldTy>) -> DynResult<DynValue> {
        let id = ft.type_id;
        if let Some(p) = Prim::from_type_id(id) {
            return self.read_prim(p);
        }
        self.enter()?;
        let v = match id {
            tid::LIST | tid::SET => self.read_collection(ft.generics.first(), hint, id == tid::SET),
            tid::MAP => self.read_map(ft.generics.first(), ft.generics.get(1), hint),
            tid::ENUM | tid::NAMED_ENUM => {
                let local = hint_index(hint, |k| matches!(k, Kind::Enum(_)));
                self.read_enum(local, None)
            }
            tid::UNION | tid::TYPED_UNION | tid::NAMED_UNION => {
                let local = hint_index(hint, |k| matches!(k, Kind::Union(_)));
                self.read_union(local, None)
            }
            tid::STRUCT
            | tid::COMPATIBLE_STRUCT
            | tid::NAMED_STRUCT
            | tid::NAMED_COMPATIBLE_STRUCT
            | tid::EXT
            | tid::NAMED_EXT
            | tid::UNKNOWN => {
                let info = self.read_type_info()?;
                self.read_with_info_inner(&info, hint)
            }
            _ => self.read_primitive_array(id),
        };
        self.depth -= 1;
        v
    }

    fn read_struct(&mut self, meta: &Rc<WireMeta>) -> DynResult<DynValue> {
        let schema = self.schema;
        let local = meta.local.map(|i| &schema.types[i]);
        let mut wire_values: Vec<(String, DynValue)> = Vec::with_capacity(meta.fields.len());
        for (name, ft) in &meta.fields {
            let local_field = local.and_then(|t| t.field_by_name(name)).map(|(_, f)| f);
            let v = self
                .read_field(ft, local_field.map(|f| &f.ty))
                .map_err(|e| e.at(&format!(".{name}")))?;
            wire_values.push((name.clone(), v));
        }
        let Some(t) = local else {
            return Ok(DynValue::Struct(DynStruct {
                type_name: meta.full_name(),
                fields: wire_values,
            }));
        };
        let Body::Message { fields, .. } = &t.body else {
            return Err(DynError::decode(format!(
                "{} is a struct on the wire but not in the local schema",
                t.full_name
            )));
        };
        let mut taken = vec![false; wire_values.len()];
        let mut out = Vec::with_capacity(fields.len() + wire_values.len());
        for f in fields {
            let pos = wire_values
                .iter()
                .position(|(n, _)| *n == f.wire_name || *n == f.name);
            match pos {
                Some(i) => {
                    taken[i] = true;
                    out.push((
                        f.name.clone(),
                        std::mem::replace(&mut wire_values[i].1, DynValue::Null),
                    ));
                }
                None if self.opts.fill_missing => {
                    let v = match &f.default {
                        Some(d) => d.clone(),
                        None => schema.zero_value(&f.ty, self.depth)?,
                    };
                    out.push((f.name.clone(), v));
                }
                None => {}
            }
        }
        if self.opts.keep_unknown_fields {
            for (i, (n, v)) in wire_values.into_iter().enumerate() {
                if !taken[i] {
                    out.push((n, v));
                }
            }
        }
        Ok(DynValue::Struct(DynStruct {
            type_name: t.full_name.clone(),
            fields: out,
        }))
    }

    fn read_enum(
        &mut self,
        local: Option<usize>,
        wire_name: Option<String>,
    ) -> DynResult<DynValue> {
        let ordinal = self.r.read_var_u32()?;
        let (type_name, name) = match local.map(|i| &self.schema.types[i]) {
            Some(t) => {
                let name = match &t.body {
                    Body::Enum { values } => values
                        .iter()
                        .find(|(_, o)| *o == ordinal)
                        .map(|(n, _)| n.clone()),
                    _ => None,
                };
                (t.full_name.clone(), name)
            }
            None => (wire_name.unwrap_or_default(), None),
        };
        Ok(DynValue::Enum(DynEnum {
            type_name,
            name,
            ordinal,
        }))
    }

    fn read_union(
        &mut self,
        local: Option<usize>,
        wire_name: Option<String>,
    ) -> DynResult<DynValue> {
        let case_id = self.r.read_var_u32()?;
        let local_t = local.map(|i| &self.schema.types[i]);
        let variant = local_t.and_then(|t| match &t.body {
            Body::Union { variants } => variants.iter().find(|v| v.case_id == case_id).cloned(),
            _ => None,
        });
        let value = match self.r.read_i8()? {
            REF_FLAG_NULL => DynValue::Null,
            REF_FLAG_REF => {
                return Err(DynError::unsupported(
                    "shared references (Aster payloads run with ref tracking off)",
                ))
            }
            _ => {
                let info = self.read_type_info()?;
                self.read_with_info(&info, variant.as_ref().map(|v| &v.payload))?
            }
        };
        Ok(DynValue::Union(DynUnion {
            type_name: local_t
                .map(|t| t.full_name.clone())
                .or(wire_name)
                .unwrap_or_default(),
            case_id,
            case_name: variant.map(|v| v.name),
            value: Box::new(value),
        }))
    }

    fn check_len(&self, len: u32) -> DynResult<usize> {
        let len = len as usize;
        // Every element / entry takes at least one byte, except a struct with
        // no fields; bound the up-front allocation by the remaining input.
        if len > self.r.slice_after_cursor().len().saturating_mul(8).max(64) {
            return Err(DynError::decode(format!(
                "collection length {len} exceeds the payload"
            )));
        }
        Ok(len)
    }

    fn read_collection(
        &mut self,
        elem: Option<&FieldType>,
        hint: Option<&FieldTy>,
        is_set: bool,
    ) -> DynResult<DynValue> {
        let len = self.r.read_var_u32()?;
        let len = self.check_len(len)?;
        let wrap = |items| {
            if is_set {
                DynValue::Set(items)
            } else {
                DynValue::List(items)
            }
        };
        if len == 0 {
            return Ok(wrap(Vec::new()));
        }
        let elem_hint = hint.and_then(|h| match &h.kind {
            Kind::List(e) | Kind::Set(e) => Some((**e).clone()),
            _ => None,
        });
        let header = self.r.read_u8()?;
        let declared = header & COLL_DECL_ELEMENT_TYPE != 0;
        let same = header & COLL_IS_SAME_TYPE != 0;
        let flagged = header & (COLL_HAS_NULL | COLL_TRACKING_REF) != 0;
        let shared_info = if !declared && same {
            Some(self.read_type_info()?)
        } else {
            None
        };
        let elem = if declared {
            Some(elem.ok_or_else(|| {
                DynError::decode("list declares its element type but the field type has none")
            })?)
        } else {
            None
        };
        let mut items = Vec::with_capacity(len.min(4096));
        for i in 0..len {
            if flagged {
                match self.r.read_i8()? {
                    REF_FLAG_NULL => {
                        items.push(DynValue::Null);
                        continue;
                    }
                    REF_FLAG_REF => {
                        return Err(DynError::unsupported("shared references in a list"))
                    }
                    _ => {}
                }
            }
            let v = if let Some(ft) = elem {
                self.read_declared(ft, elem_hint.as_ref())
            } else if let Some(info) = &shared_info {
                self.read_with_info(info, elem_hint.as_ref())
            } else {
                let info = self.read_type_info()?;
                self.read_with_info(&info, elem_hint.as_ref())
            }
            .map_err(|e| e.at(&format!("[{i}]")))?;
            items.push(v);
        }
        Ok(wrap(items))
    }

    fn read_map(
        &mut self,
        kft: Option<&FieldType>,
        vft: Option<&FieldType>,
        hint: Option<&FieldTy>,
    ) -> DynResult<DynValue> {
        let size = self.r.read_var_u32()?;
        let size = self.check_len(size)?;
        let (khint, vhint) = match hint.map(|h| &h.kind) {
            Some(Kind::Map(k, v)) => (Some((**k).clone()), Some((**v).clone())),
            _ => (None, None),
        };
        let mut entries = Vec::with_capacity(size.min(4096));
        while entries.len() < size {
            let header = self.r.read_u8()?;
            let key_null = header & MAP_KEY_NULL != 0;
            let value_null = header & MAP_VALUE_NULL != 0;
            if key_null || value_null {
                let k = if key_null {
                    DynValue::Null
                } else {
                    self.read_map_slot(
                        header & MAP_DECL_KEY_TYPE != 0,
                        header & MAP_TRACKING_KEY_REF != 0,
                        kft,
                        khint.as_ref(),
                    )?
                };
                let v = if value_null {
                    DynValue::Null
                } else {
                    self.read_map_slot(
                        header & MAP_DECL_VALUE_TYPE != 0,
                        header & MAP_TRACKING_VALUE_REF != 0,
                        vft,
                        vhint.as_ref(),
                    )?
                };
                entries.push((k, v));
                continue;
            }
            let count = self.r.read_u8()? as usize;
            if count == 0 || entries.len() + count > size {
                return Err(DynError::decode("bad map chunk size"));
            }
            let kinfo = if header & MAP_DECL_KEY_TYPE == 0 {
                Some(self.read_type_info()?)
            } else {
                None
            };
            let vinfo = if header & MAP_DECL_VALUE_TYPE == 0 {
                Some(self.read_type_info()?)
            } else {
                None
            };
            for _ in 0..count {
                let k = self.read_chunk_slot(
                    header & MAP_TRACKING_KEY_REF != 0,
                    kinfo.as_ref(),
                    kft,
                    khint.as_ref(),
                )?;
                let v = self.read_chunk_slot(
                    header & MAP_TRACKING_VALUE_REF != 0,
                    vinfo.as_ref(),
                    vft,
                    vhint.as_ref(),
                )?;
                entries.push((k, v));
            }
        }
        Ok(DynValue::Map(entries))
    }

    fn read_ref_flag(&mut self) -> DynResult<bool> {
        match self.r.read_i8()? {
            REF_FLAG_NULL => Ok(false),
            REF_FLAG_REF => Err(DynError::unsupported("shared references in a map")),
            _ => Ok(true),
        }
    }

    fn read_map_slot(
        &mut self,
        declared: bool,
        tracking: bool,
        ft: Option<&FieldType>,
        hint: Option<&FieldTy>,
    ) -> DynResult<DynValue> {
        if tracking && !self.read_ref_flag()? {
            return Ok(DynValue::Null);
        }
        if declared {
            let ft =
                ft.ok_or_else(|| DynError::decode("map declares a slot type it does not have"))?;
            self.read_declared(ft, hint)
        } else {
            let info = self.read_type_info()?;
            self.read_with_info(&info, hint)
        }
    }

    fn read_chunk_slot(
        &mut self,
        tracking: bool,
        info: Option<&TypeInfo>,
        ft: Option<&FieldType>,
        hint: Option<&FieldTy>,
    ) -> DynResult<DynValue> {
        if tracking && !self.read_ref_flag()? {
            return Ok(DynValue::Null);
        }
        match info {
            Some(info) => self.read_with_info(info, hint),
            None => {
                let ft = ft
                    .ok_or_else(|| DynError::decode("map declares a slot type it does not have"))?;
                self.read_declared(ft, hint)
            }
        }
    }

    fn read_prim(&mut self, p: Prim) -> DynResult<DynValue> {
        let r = &mut self.r;
        Ok(match p {
            Prim::Bool => DynValue::Bool(r.read_u8()? != 0),
            Prim::Int8 => DynValue::I8(r.read_i8()?),
            Prim::Int16 => DynValue::I16(r.read_i16()?),
            Prim::Int32 => DynValue::I32(r.read_i32()?),
            Prim::VarInt32 => DynValue::I32(r.read_var_i32()?),
            Prim::Int64 => DynValue::I64(r.read_i64()?),
            Prim::VarInt64 => DynValue::I64(r.read_var_i64()?),
            Prim::TaggedInt64 => DynValue::I64(r.read_tagged_i64()?),
            Prim::UInt8 => DynValue::U8(r.read_u8()?),
            Prim::UInt16 => DynValue::U16(r.read_u16()?),
            Prim::UInt32 => DynValue::U32(r.read_u32()?),
            Prim::VarUInt32 => DynValue::U32(r.read_var_u32()?),
            Prim::UInt64 => DynValue::U64(r.read_u64()?),
            Prim::VarUInt64 => DynValue::U64(r.read_var_u64()?),
            Prim::TaggedUInt64 => DynValue::U64(r.read_tagged_u64()?),
            Prim::Float16 => DynValue::F32(r.read_f16()?.to_f32()),
            Prim::BFloat16 => DynValue::F32(r.read_bf16()?.to_f32()),
            Prim::Float32 => DynValue::F32(r.read_f32()?),
            Prim::Float64 => DynValue::F64(r.read_f64()?),
            Prim::String => {
                let header = r.read_var_u36_small()?;
                let len = usize::try_from(header >> 2)
                    .map_err(|_| DynError::decode("string length overflow"))?;
                DynValue::String(match header & 0b11 {
                    0 => r.read_latin1_string(len)?,
                    1 => {
                        if len % 2 != 0 {
                            return Err(DynError::decode("odd UTF-16 byte length"));
                        }
                        r.read_utf16_string(len)?
                    }
                    2 => r.read_utf8_string(len)?,
                    e => return Err(DynError::decode(format!("string encoding {e}"))),
                })
            }
            Prim::Binary => {
                let len = r.read_var_u32()? as usize;
                DynValue::Bytes(r.read_bytes(len)?.to_vec())
            }
            Prim::Timestamp => {
                let seconds = r.read_i64()?;
                let nanos = r.read_u32()?;
                DynValue::Timestamp { seconds, nanos }
            }
            Prim::Date => DynValue::Date {
                days: r.read_var_i64()?,
            },
            Prim::Duration => {
                let seconds = r.read_var_i64()?;
                let nanos = r.read_i32()?;
                DynValue::Duration { seconds, nanos }
            }
        })
    }

    /// Dense primitive arrays (`int32_array` etc.): byte length + raw values.
    fn read_primitive_array(&mut self, id: u32) -> DynResult<DynValue> {
        let width = match id {
            tid::BOOL_ARRAY | tid::INT8_ARRAY | tid::UINT8_ARRAY => 1,
            tid::INT16_ARRAY | tid::UINT16_ARRAY | tid::FLOAT16_ARRAY | tid::BFLOAT16_ARRAY => 2,
            tid::INT32_ARRAY | tid::UINT32_ARRAY | tid::FLOAT32_ARRAY => 4,
            tid::INT64_ARRAY | tid::UINT64_ARRAY | tid::FLOAT64_ARRAY => 8,
            _ => {
                return Err(DynError::unsupported(format!(
                    "Fory type id {id} has no dynamic decoding"
                )))
            }
        };
        let len = self.r.read_var_u32()? as usize;
        if !len.is_multiple_of(width) {
            return Err(DynError::decode(
                "primitive array length is not a multiple of its width",
            ));
        }
        let raw = self.r.read_bytes(len)?;
        if id == tid::UINT8_ARRAY {
            return Ok(DynValue::Bytes(raw.to_vec()));
        }
        let mut out = Vec::with_capacity(len / width);
        for c in raw.chunks_exact(width) {
            out.push(match id {
                tid::BOOL_ARRAY => DynValue::Bool(c[0] != 0),
                tid::INT8_ARRAY => DynValue::I8(c[0] as i8),
                tid::INT16_ARRAY => DynValue::I16(i16::from_le_bytes([c[0], c[1]])),
                tid::UINT16_ARRAY => DynValue::U16(u16::from_le_bytes([c[0], c[1]])),
                tid::FLOAT16_ARRAY => DynValue::F32(
                    fory_core::Float16::from_bits(u16::from_le_bytes([c[0], c[1]])).to_f32(),
                ),
                tid::BFLOAT16_ARRAY => DynValue::F32(f32::from_bits(
                    (u16::from_le_bytes([c[0], c[1]]) as u32) << 16,
                )),
                tid::INT32_ARRAY => DynValue::I32(i32::from_le_bytes(c.try_into().unwrap())),
                tid::UINT32_ARRAY => DynValue::U32(u32::from_le_bytes(c.try_into().unwrap())),
                tid::FLOAT32_ARRAY => DynValue::F32(f32::from_le_bytes(c.try_into().unwrap())),
                tid::INT64_ARRAY => DynValue::I64(i64::from_le_bytes(c.try_into().unwrap())),
                tid::UINT64_ARRAY => DynValue::U64(u64::from_le_bytes(c.try_into().unwrap())),
                tid::FLOAT64_ARRAY => DynValue::F64(f64::from_le_bytes(c.try_into().unwrap())),
                _ => unreachable!(),
            });
        }
        Ok(DynValue::List(out))
    }
}

fn hint_index(hint: Option<&FieldTy>, want: impl Fn(&Kind) -> bool) -> Option<usize> {
    let h = hint?;
    if !want(&h.kind) {
        return None;
    }
    match h.kind {
        Kind::Enum(i) | Kind::Union(i) | Kind::Struct(i) => Some(i),
        _ => None,
    }
}

/// The 52-bit hash in a TypeDef header covers the body (fory-core
/// `type_meta_hash_bits`).
fn check_meta_hash(header: i64, body: &[u8]) -> DynResult<()> {
    let low = (header as u64) & ((1u64 << META_HASH_SHIFT) - 1);
    let mut input = Vec::with_capacity(body.len() + 2);
    input.extend_from_slice(body);
    input.push(low as u8);
    input.push((low >> 8) as u8);
    let h = murmurhash3_x64_128(&input, 47).0 as i64;
    let expected = h.wrapping_shl(META_HASH_SHIFT).wrapping_abs() as u64;
    let mask = u64::MAX << META_HASH_SHIFT;
    if (header as u64) & mask != expected & mask {
        return Err(DynError::decode("TypeDef hash mismatch"));
    }
    Ok(())
}
