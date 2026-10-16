//! Proc-macros for the Aster RPC framework.
//!
//! `#[derive(AsterType)]` generates the [`AsterType`] + `WireField` impls for an
//! RPC payload struct: it emits a `core::contract::TypeDef` (via the `aster::rpc`
//! runtime builders) whose canonical bytes / BLAKE3 hash match the Aster
//! ContractIdentity spec (§11.3), so contract-ids are cross-binding-stable.
//!
//! Attributes:
//! - `#[aster(wire = "namespace/Name")]` (struct): the wire tag → TypeDef
//!   `package`/`name`. Recommended for cross-binding stability; defaults to
//!   `package = ""`, `name = <StructName>` if omitted.
//! - `#[aster(default = <expr>)]` (field): a scalar default value, lowered into
//!   the contract per §11.3.2.3 (`required = false`, `default_value` = canonical
//!   bytes of the default). Supported for `String` / `bool` / `i32` / `i64` /
//!   `f64`.
//! - `#[aster(default)]` (field, bare): an empty-container default (`list`/`set`/
//!   `map`) — `required = false`, `default_value` = the `0x00` sentinel.
//! - `#[aster(uint8_list)]` (field): keep a `Vec<u8>` / `VecDeque<u8>` as a Fory
//!   list and publish it as `list<uint8>` instead of `binary`.
//! - `#[aster(uuid)]` / `#[aster(ulid)]` (field): publish the field's `[u8; 16]`
//!   leaves (the field itself, an `Option`, list / set elements, map keys or
//!   values) as `uuid` / `ulid` instead of `binary`. The wire stays Fory
//!   BINARY of exactly 16 bytes, which Fory's `[u8; 16]` decoder enforces.
//!   `aster::rpc::{Uuid, Ulid}` convert to and from the array and parse /
//!   format the string forms.
//!
//! Field types: scalars, `aster::rpc::{Timestamp, Date, Decimal}` (published as
//! `timestamp` / `date` / `decimal`), `aster::rpc::FixedDecimal<P, S>`
//! (`decimal(P,S)`), other `AsterType`s, and `Option` / `Vec`
//! / sets / maps of them. `Box<T>` is transparent. Types may be recursive or
//! mutually recursive: identity follows the spec's SCC rule (§11.3.4 Step 3),
//! resolved at runtime by `aster::rpc::resolve_type_def`.
//!
//! Byte fields: a byte buffer is published as `binary` (Python `bytes`, TS
//! `Uint8Array`), so its Fory encoding must be BINARY too. `[u8; N]` already is;
//! a `Vec<u8>` is only with `#[fory(bytes)]` (`list(element(bytes))` /
//! `map(key(bytes))` / `map(value(bytes))` when nested). An unannotated
//! `Vec<u8>` is a compile error — the derive cannot add the attribute itself.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, Data, DeriveInput, Expr, ExprLit, Fields, FnArg, GenericArgument, Ident,
    ItemTrait, Lit, MetaNameValue, PathArguments, ReturnType, Token, TraitItem, Type,
};

#[proc_macro_derive(AsterType, attributes(aster))]
pub fn derive_aster_type(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let (package, type_name) = parse_wire_tag(&input.attrs, &ident.to_string())?;

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => &n.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    ident,
                    "#[derive(AsterType)] requires a struct with named fields",
                ))
            }
        },
        Data::Enum(e) => return expand_union(ident, &package, &type_name, e),
        _ => {
            return Err(syn::Error::new_spanned(
                ident,
                "#[derive(AsterType)] can only be applied to structs (messages) or \
                 data-carrying enums (unions)",
            ))
        }
    };

    let mut field_exprs: Vec<TokenStream2> = Vec::new();
    // Transitive Fory registration: recurse into each field's element/value/key
    // type. Scalars resolve to the `WireField` no-op default; nested structs
    // register themselves and recurse further.
    let mut register_children: Vec<TokenStream2> = Vec::new();
    let mut collect_children: Vec<TokenStream2> = Vec::new();
    for f in fields {
        let fname = f.ident.as_ref().unwrap().to_string();
        let info = analyze_field(f)?;
        let default_expr = parse_default(&f.attrs, &info)?;
        let leaf = &info.leaf_expr;
        let key = match &info.key_expr {
            Some(k) => quote!(::core::option::Option::Some(#k)),
            None => quote!(::core::option::Option::None),
        };
        let optional = info.optional;
        let container = info.container;
        field_exprs.push(quote! {
            ::aster::rpc::shape_field(#fname, #leaf, #optional, #container, #key, #default_expr)
        });
        for child in child_types(&f.ty) {
            register_children.push(quote! {
                <#child as ::aster::rpc::WireField>::register_payload(__reg);
            });
            collect_children.push(quote! {
                <#child as ::aster::rpc::WireField>::collect_type_defs(__set);
            });
        }
    }

    Ok(quote! {
        impl ::aster::rpc::AsterType for #ident {
            fn aster_type_def() -> ::aster::rpc::TypeDef {
                ::aster::rpc::resolve_type_def(<Self as ::aster::rpc::AsterType>::aster_type_shape)
            }

            fn aster_type_shape() -> ::aster::rpc::TypeShape {
                ::aster::rpc::message_shape(
                    #package,
                    #type_name,
                    ::std::vec![ #(#field_exprs),* ],
                )
            }
        }

        impl ::aster::rpc::WireField for #ident {
            fn leaf() -> ::aster::rpc::Leaf {
                ::aster::rpc::Leaf::named(<Self as ::aster::rpc::AsterType>::aster_type_shape)
            }

            fn register_payload(__reg: &mut ::aster::rpc::PayloadRegistry) {
                if !__reg.mark_seen::<Self>() {
                    return;
                }
                let __td = <Self as ::aster::rpc::AsterType>::aster_type_def();
                __reg.register::<Self>(&::std::format!("{}.{}", __td.package, __td.name));
                #(#register_children)*
            }

            fn collect_type_defs(__set: &mut ::aster::rpc::TypeDefSet) {
                if !__set.mark_seen::<Self>() {
                    return;
                }
                __set.push(<Self as ::aster::rpc::AsterType>::aster_type_def());
                #(#collect_children)*
            }
        }
    })
}

/// Expand `#[derive(AsterType)]` on a data-carrying enum → a `TypeDefKind::Union`
/// contract type (§11.3.3 v1 scope):
///
/// - Every variant must have exactly one unnamed field whose type is itself an
///   `AsterType` **message** (primitive variants like `Int(i64)` are outside
///   the v1 cross-binding contract scope — wrap the primitive in a one-field
///   message).
/// - `T | nothing` is not a union variant — model nullability as
///   `Option<TheUnion>` at the *field* using the union (§11.3.3 rule 2), so
///   unit variants are rejected.
/// - Variant case ids follow the `ForyUnion` scheme (explicit `#[fory(id = N)]`,
///   else 0-based declaration index over the known variants) so contract
///   identity and Fory wire discriminators agree. Pair with
///   `#[derive(ForyUnion)]` for serialization — which also requires a trailing
///   `#[fory(unknown)] Unknown(UnknownCase)` forward-compat variant; that
///   variant is a runtime artifact and is **excluded** from the contract.
fn expand_union(
    ident: &Ident,
    package: &str,
    type_name: &str,
    data: &syn::DataEnum,
) -> syn::Result<TokenStream2> {
    let mut variant_exprs: Vec<TokenStream2> = Vec::new();
    let mut register_children: Vec<TokenStream2> = Vec::new();
    let mut collect_children: Vec<TokenStream2> = Vec::new();
    let mut implicit_id: i32 = 0;
    for variant in data.variants.iter() {
        // `#[fory(unknown)] Unknown(UnknownCase)` — ForyUnion's runtime
        // forward-compat carrier. Not a wire variant; skip it in the contract
        // (and in implicit-id assignment, mirroring ForyUnion).
        if has_fory_unknown_attr(variant) {
            continue;
        }
        let vname = variant.ident.to_string();
        let payload = match &variant.fields {
            Fields::Unnamed(u) if u.unnamed.len() == 1 => &u.unnamed.first().unwrap().ty,
            Fields::Unit => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "AsterType unions cannot carry unit variants: `nothing` is not a \
                     cross-binding variant type. Model absence as Option<TheUnion> on the \
                     field that uses this union, or wrap an empty message",
                ))
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "AsterType union variants must have exactly one unnamed field whose type \
                     is an AsterType message, e.g. `Int(IntValue)` — primitive payloads like \
                     `Int(i64)` are outside the v1 contract scope (§11.3.3)",
                ))
            }
        };
        let id = fory_variant_id(variant)?.unwrap_or(implicit_id);
        implicit_id += 1;
        variant_exprs.push(quote! {
            ::aster::rpc::shape_variant(
                #vname,
                #id,
                ::aster::rpc::Leaf::named(<#payload as ::aster::rpc::AsterType>::aster_type_shape),
            )
        });
        register_children.push(quote! {
            <#payload as ::aster::rpc::WireField>::register_payload(__reg);
        });
        collect_children.push(quote! {
            <#payload as ::aster::rpc::WireField>::collect_type_defs(__set);
        });
    }

    Ok(quote! {
        impl ::aster::rpc::AsterType for #ident {
            fn aster_type_def() -> ::aster::rpc::TypeDef {
                ::aster::rpc::resolve_type_def(<Self as ::aster::rpc::AsterType>::aster_type_shape)
            }

            fn aster_type_shape() -> ::aster::rpc::TypeShape {
                ::aster::rpc::union_shape(
                    #package,
                    #type_name,
                    ::std::vec![ #(#variant_exprs),* ],
                )
            }
        }

        impl ::aster::rpc::WireField for #ident {
            fn leaf() -> ::aster::rpc::Leaf {
                ::aster::rpc::Leaf::named(<Self as ::aster::rpc::AsterType>::aster_type_shape)
            }

            fn register_payload(__reg: &mut ::aster::rpc::PayloadRegistry) {
                if !__reg.mark_seen::<Self>() {
                    return;
                }
                let __td = <Self as ::aster::rpc::AsterType>::aster_type_def();
                __reg.register_union::<Self>(&::std::format!("{}.{}", __td.package, __td.name));
                #(#register_children)*
            }

            fn collect_type_defs(__set: &mut ::aster::rpc::TypeDefSet) {
                if !__set.mark_seen::<Self>() {
                    return;
                }
                __set.push(<Self as ::aster::rpc::AsterType>::aster_type_def());
                #(#collect_children)*
            }
        }
    })
}

/// Whether a union variant carries `#[fory(unknown)]` — ForyUnion's runtime
/// forward-compat marker. (ForyUnion itself validates the full shape; here the
/// attribute alone decides contract exclusion.)
fn has_fory_unknown_attr(variant: &syn::Variant) -> bool {
    variant.attrs.iter().any(|attr| {
        if !attr.path().is_ident("fory") {
            return false;
        }
        let mut found = false;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("unknown") {
                found = true;
            } else if meta.input.peek(syn::Token![=]) {
                let _: Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
        found
    })
}

/// Read an explicit `#[fory(id = N)]` case id off a union variant (the same
/// attribute `ForyUnion` reads), so the contract mirrors the wire discriminator.
fn fory_variant_id(variant: &syn::Variant) -> syn::Result<Option<i32>> {
    for attr in &variant.attrs {
        if !attr.path().is_ident("fory") {
            continue;
        }
        let mut id: Option<i32> = None;
        // Tolerate other #[fory(...)] keys (skip, default, …) — only `id`
        // matters for contract identity.
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("id") {
                if let Ok(value) = meta.value() {
                    if let Ok(lit) = value.parse::<syn::LitInt>() {
                        id = Some(lit.base10_parse::<i32>()?);
                    }
                }
            } else if meta.input.peek(syn::Token![=]) {
                let _: Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
        if id.is_some() {
            return Ok(id);
        }
    }
    Ok(None)
}

/// Parse `#[aster(wire = "ns/Name")]` → `(package, name)`. Splits on the last
/// `/`. Defaults to `("", <struct ident>)` when absent.
fn parse_wire_tag(attrs: &[syn::Attribute], ident: &str) -> syn::Result<(String, String)> {
    let mut wire: Option<(String, proc_macro2::Span)> = None;
    for attr in attrs {
        if !attr.path().is_ident("aster") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("wire") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                wire = Some((lit.value(), lit.span()));
                Ok(())
            } else {
                // Ignore unknown struct-level keys here; field-level keys are
                // parsed elsewhere.
                Err(meta.error("unsupported #[aster(...)] key on a struct"))
            }
        })?;
    }
    match wire {
        Some((tag, span)) => {
            let (package, name) = match tag.rsplit_once('/') {
                Some((ns, name)) => (ns.to_string(), name.to_string()),
                None => (String::new(), tag),
            };
            if is_reserved_namespace(&package) && !expanding_inside_aster() {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "wire package `{package}` is reserved: the `aster` namespace is for \
                         baseline types shipped with the framework. Use your own package, \
                         e.g. `#[aster(wire = \"myapp/{name}\")]`",
                    ),
                ));
            }
            Ok((package, name))
        }
        None => Ok((String::new(), ident.to_string())),
    }
}

/// Whether `value` falls in the reserved `aster` namespace: the bare package
/// `aster`, or anything under `aster.` / `aster/`. Deliberately does NOT match
/// `aster-foo` / `asterix` — only the exact namespace is reserved.
fn is_reserved_namespace(value: &str) -> bool {
    value == "aster" || value.starts_with("aster.") || value.starts_with("aster/")
}

/// Escape hatch for the framework itself: baseline services/types defined in
/// the `aster` crate are allowed to use the reserved namespace. Checked at
/// macro-expansion time via the expanding crate's package name (covers the
/// crate's own tests/examples too, which share `CARGO_PKG_NAME=aster`).
fn expanding_inside_aster() -> bool {
    std::env::var("CARGO_PKG_NAME").is_ok_and(|p| FIRST_PARTY.contains(&p.as_str()))
}

/// Crates that may use the reserved namespace: the framework itself, and the
/// client of the @aster directory service, whose Python server (`aster-app`)
/// declares its types as `aster/<Type>`.
const FIRST_PARTY: &[&str] = &["aster", "aster-directory-client"];

struct FieldInfo {
    optional: bool,
    /// 0 = None, 1 = List, 2 = Set, 3 = Map.
    container: u8,
    /// Expression producing the leaf `Leaf` (element/value type, or the field
    /// type itself when there's no container).
    leaf_expr: TokenStream2,
    /// Map key `Leaf` expression (Map only).
    key_expr: Option<TokenStream2>,
    /// The scalar type a `#[aster(default = ...)]` would target, if this field
    /// can carry a scalar default (no container). `None` for container fields.
    default_ty: Option<Type>,
}

// ── Byte fields: keep the contract and the Fory wire in agreement ─────────────
//
// The contract publishes a byte buffer as `binary`, which is what Python
// `bytes` and TypeScript `Uint8Array` put on the wire (Fory BINARY). Fory's
// Rust derive, however, writes an *unannotated* `Vec<u8>` as LIST<UINT8>; only
// `#[fory(bytes)]` makes it BINARY. A derive macro cannot add attributes to the
// struct it is applied to, so `AsterType` cannot switch the encoding on by
// itself. Instead it reads the field's `#[fory(...)]` config (the same
// attributes `ForyStruct` reads) and refuses to compile when the published
// contract type and the Fory encoding would disagree.

/// The subset of a field's `#[fory(...)]` config that decides how byte buffers
/// are encoded: `bytes` / `array` at this value node, plus the nested
/// `list(element(...))` and `map(key(...), value(...))` nodes. Everything else
/// (`id`, `nullable`, `encoding`, …) is irrelevant here and skipped.
#[derive(Default, Debug)]
struct ForyNode {
    bytes: bool,
    array: bool,
    element: Option<Box<ForyNode>>,
    key: Option<Box<ForyNode>>,
    value: Option<Box<ForyNode>>,
}

impl ForyNode {
    /// Merge every `#[fory(...)]` attribute on a field into one node.
    fn from_attrs(attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut node = ForyNode::default();
        for attr in attrs {
            if attr.path().is_ident("fory") {
                attr.parse_nested_meta(|meta| node.parse_item(meta))?;
            }
        }
        Ok(node)
    }

    fn parse_item(&mut self, meta: syn::meta::ParseNestedMeta) -> syn::Result<()> {
        let child = |slot: &mut Option<Box<ForyNode>>, m: syn::meta::ParseNestedMeta| {
            let node = slot.get_or_insert_with(Default::default);
            m.parse_nested_meta(|c| node.parse_item(c))
        };
        if meta.path.is_ident("bytes") {
            self.bytes = true;
        } else if meta.path.is_ident("array") {
            self.array = true;
        } else if meta.path.is_ident("list") {
            meta.parse_nested_meta(|m| {
                if m.path.is_ident("element") {
                    child(&mut self.element, m)
                } else {
                    skip_meta(&m)
                }
            })?;
        } else if meta.path.is_ident("map") {
            meta.parse_nested_meta(|m| {
                if m.path.is_ident("key") {
                    child(&mut self.key, m)
                } else if m.path.is_ident("value") {
                    child(&mut self.value, m)
                } else {
                    skip_meta(&m)
                }
            })?;
        } else {
            skip_meta(&meta)?;
        }
        Ok(())
    }

    fn element(&self) -> Option<&ForyNode> {
        self.element.as_deref()
    }
    fn key(&self) -> Option<&ForyNode> {
        self.key.as_deref()
    }
    fn value(&self) -> Option<&ForyNode> {
        self.value.as_deref()
    }
}

/// Consume a `#[fory(...)]` item this derive does not interpret
/// (`id = 3`, `nullable`, `tuple(...)`, `with = path`, …). `ForyStruct`
/// validates them; here they only need to be stepped over.
fn skip_meta(meta: &syn::meta::ParseNestedMeta) -> syn::Result<()> {
    if meta.input.peek(Token![=]) {
        let _: Expr = meta.value()?.parse()?;
    } else if meta.input.peek(syn::token::Paren) {
        let content;
        syn::parenthesized!(content in meta.input);
        let _: TokenStream2 = content.parse()?;
    }
    Ok(())
}

/// The field-level `#[aster(...)]` flags that change how a byte leaf is
/// published.
#[derive(Default)]
struct FieldOpts {
    /// `#[aster(uint8_list)]` — the explicit opt-out that keeps a `Vec<u8>` as
    /// a Fory LIST<UINT8> and publishes it as `list<uint8>` instead of
    /// `binary`.
    uint8_list: bool,
    /// `#[aster(uuid)]` / `#[aster(ulid)]` — publish `[u8; 16]` leaves as this
    /// canonical identifier type instead of `binary`.
    id16: Option<&'static str>,
    /// Set once an `id16` leaf has been published, so an attribute that
    /// matched nothing is an error.
    id16_used: std::cell::Cell<bool>,
}

fn field_opts(attrs: &[syn::Attribute]) -> syn::Result<FieldOpts> {
    let mut opts = FieldOpts::default();
    for attr in attrs {
        if !attr.path().is_ident("aster") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("uint8_list") {
                opts.uint8_list = true;
            } else if meta.path.is_ident("uuid") || meta.path.is_ident("ulid") {
                let id = if meta.path.is_ident("uuid") {
                    "uuid"
                } else {
                    "ulid"
                };
                if opts.id16.is_some_and(|prev| prev != id) {
                    return Err(meta.error("a field is either `uuid` or `ulid`, not both"));
                }
                opts.id16 = Some(id);
            } else {
                // `default` etc. are parsed by `parse_default`.
                skip_meta(&meta)?;
            }
            Ok(())
        })?;
    }
    Ok(opts)
}

/// Whether `ty` is `[u8; 16]` (the length written as the literal `16`).
fn is_u8_16(ty: &Type) -> bool {
    match ty {
        Type::Array(a) => {
            is_u8(&a.elem)
                && matches!(&a.len, Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. })
                    if n.base10_parse::<usize>().ok() == Some(16))
        }
        Type::Paren(p) => is_u8_16(&p.elem),
        Type::Group(g) => is_u8_16(&g.elem),
        _ => false,
    }
}

/// The Rust byte-buffer shapes a contract `binary` could map to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteShape {
    /// `Vec<u8>`: BINARY only under `#[fory(bytes)]`, LIST<UINT8> otherwise.
    Vec,
    /// `VecDeque<u8>`: Fory can only write it as LIST<UINT8>.
    VecDeque,
    /// `[u8; N]`: Fory writes a fixed `u8` array as BINARY (exactly N bytes
    /// on decode).
    Array,
}

fn byte_shape(ty: &Type) -> Option<ByteShape> {
    match ty {
        Type::Array(a) if is_u8(&a.elem) => Some(ByteShape::Array),
        Type::Paren(p) => byte_shape(&p.elem),
        Type::Group(g) => byte_shape(&g.elem),
        _ => match path_segment(ty) {
            Some((id, args)) if args.len() == 1 && is_u8(&args[0]) => match id.as_str() {
                "Vec" => Some(ByteShape::Vec),
                "VecDeque" => Some(ByteShape::VecDeque),
                _ => None,
            },
            _ => None,
        },
    }
}

/// Where a byte buffer sits in the field type, for the error message's
/// suggested attribute.
#[derive(Clone, Copy)]
enum BytePos {
    Field,
    ListElement,
    MapKey,
    MapValue,
}

impl BytePos {
    fn fory_attr(self) -> &'static str {
        match self {
            BytePos::Field => "#[fory(bytes)]",
            BytePos::ListElement => "#[fory(list(element(bytes)))]",
            BytePos::MapKey => "#[fory(map(key(bytes)))]",
            BytePos::MapValue => "#[fory(map(value(bytes)))]",
        }
    }
}

/// The contract leaf for a byte buffer at `pos`, or a compile error when the
/// Fory encoding selected by `node` would not be BINARY.
fn binary_leaf(
    ty: &Type,
    shape: ByteShape,
    node: Option<&ForyNode>,
    pos: BytePos,
    opts: &FieldOpts,
) -> syn::Result<TokenStream2> {
    if let Some(id) = opts.id16 {
        if shape == ByteShape::Array && is_u8_16(ty) {
            opts.id16_used.set(true);
            return Ok(quote!(::aster::rpc::Leaf::primitive(#id)));
        }
        return Err(syn::Error::new_spanned(
            ty,
            format!(
                "`#[aster({id})]` publishes the field's byte leaves as `{id}`, 16 bytes on the \
                 wire, so each must be written `[u8; 16]`"
            ),
        ));
    }
    let binary = quote!(::aster::rpc::Leaf::primitive("binary"));
    let bytes = node.is_some_and(|n| n.bytes);
    let array = node.is_some_and(|n| n.array);
    match shape {
        ByteShape::Array => Ok(binary),
        ByteShape::Vec if bytes => Ok(binary),
        ByteShape::Vec if array => Err(syn::Error::new_spanned(
            ty,
            "`#[fory(array)]` encodes `Vec<u8>` as a Fory UINT8_ARRAY, which has no Aster \
             contract type. Use `#[fory(bytes)]` to publish it as `binary`",
        )),
        ByteShape::Vec => {
            let opt_out = match pos {
                BytePos::Field => {
                    ", or `#[aster(uint8_list)]` to keep it a list and publish it as `list<uint8>`"
                }
                _ => "",
            };
            Err(syn::Error::new_spanned(
                ty,
                format!(
                    "`Vec<u8>` is published in the Aster contract as `binary` (Python `bytes`, \
                     TypeScript `Uint8Array`), but Fory serializes an unannotated `Vec<u8>` as \
                     list<uint8>, so the wire would not match the contract. Add `{}` to encode \
                     it as Fory binary{opt_out}",
                    pos.fory_attr()
                ),
            ))
        }
        ByteShape::VecDeque => Err(syn::Error::new_spanned(
            ty,
            match pos {
                BytePos::Field => {
                    "Fory can only encode `VecDeque<u8>` as list<uint8>, never as `binary`. Use \
                     `Vec<u8>` with `#[fory(bytes)]`, or mark the field `#[aster(uint8_list)]` \
                     to publish it as `list<uint8>`"
                }
                _ => {
                    "Fory can only encode `VecDeque<u8>` as list<uint8>, never as `binary`; \
                     use `Vec<u8>` with the matching `#[fory(...(bytes))]` config"
                }
            },
        )),
    }
}

/// The contract leaf for a container's element / key / value type.
fn child_leaf(
    ty: &Type,
    node: Option<&ForyNode>,
    pos: BytePos,
    opts: &FieldOpts,
) -> syn::Result<TokenStream2> {
    match byte_shape(ty) {
        Some(shape) => binary_leaf(ty, shape, node, pos, opts),
        None => Ok(leaf_of(ty)),
    }
}

/// Analyze a field into (optional, container, leaf, key). `Option<T>` sets
/// optional; a byte buffer (`Vec<u8>` + `#[fory(bytes)]`, `[u8; N]`) is
/// `binary`; `Vec`/`VecDeque`/sets/maps become containers; everything else
/// resolves its leaf via the `WireField` impl.
fn analyze_field(f: &syn::Field) -> syn::Result<FieldInfo> {
    let fory = ForyNode::from_attrs(&f.attrs)?;
    let opts = field_opts(&f.attrs)?;
    if opts.uint8_list && opts.id16.is_some() {
        return Err(syn::Error::new_spanned(
            &f.ty,
            "`#[aster(uint8_list)]` and `#[aster(uuid)]` / `#[aster(ulid)]` contradict each other",
        ));
    }
    let info = analyze_type(&f.ty, &fory, &opts)?;
    if let (Some(id), false) = (opts.id16, opts.id16_used.get()) {
        return Err(syn::Error::new_spanned(
            &f.ty,
            format!(
                "`#[aster({id})]` applies only to a `[u8; 16]` field, or an `Option`, `Vec`, \
                 set or map of `[u8; 16]`"
            ),
        ));
    }
    Ok(info)
}

fn analyze_type(ty: &Type, fory: &ForyNode, opts: &FieldOpts) -> syn::Result<FieldInfo> {
    let ty = &unbox(ty);
    if let Some((ident, args)) = path_segment(ty) {
        if ident == "Option" {
            if let Some(inner) = args.first() {
                // `Option` is transparent to Fory's field config: the
                // attributes apply to the inner type.
                let mut info = analyze_inner(&unbox(inner), fory, opts)?;
                info.optional = true;
                return Ok(info);
            }
        }
    }
    analyze_inner(ty, fory, opts)
}

/// Strip `Box<…>` layers: a box is transparent on the Fory wire and in the
/// contract (it is how a struct holds itself, `Option<Box<Self>>`). Boxes in
/// element / value / key position resolve through `WireField for Box<T>`.
fn unbox(ty: &Type) -> Type {
    match path_segment(ty) {
        Some((ident, args)) if ident == "Box" && args.len() == 1 => unbox(&args[0]),
        _ => ty.clone(),
    }
}

fn analyze_inner(ty: &Type, fory: &ForyNode, opts: &FieldOpts) -> syn::Result<FieldInfo> {
    let shape = byte_shape(ty);
    if opts.uint8_list {
        return match shape {
            Some(ByteShape::Vec | ByteShape::VecDeque) if !fory.bytes && !fory.array => {
                Ok(FieldInfo {
                    optional: false,
                    container: 1,
                    leaf_expr: quote!(::aster::rpc::Leaf::primitive("uint8")),
                    key_expr: None,
                    default_ty: None,
                })
            }
            Some(ByteShape::Vec | ByteShape::VecDeque) => Err(syn::Error::new_spanned(
                ty,
                "`#[aster(uint8_list)]` publishes the field as `list<uint8>`, which contradicts \
                 `#[fory(bytes)]` / `#[fory(array)]`; keep exactly one",
            )),
            _ => Err(syn::Error::new_spanned(
                ty,
                "`#[aster(uint8_list)]` applies only to a `Vec<u8>` / `VecDeque<u8>` field \
                 (optionally wrapped in `Option`)",
            )),
        };
    }
    if let Some(shape) = shape {
        return Ok(FieldInfo {
            optional: false,
            container: 0,
            leaf_expr: binary_leaf(ty, shape, Some(fory), BytePos::Field, opts)?,
            key_expr: None,
            default_ty: None,
        });
    }
    if let Some((ident, args)) = path_segment(ty) {
        match ident.as_str() {
            "Vec" | "VecDeque" => {
                let elem = args.first().ok_or_else(|| {
                    syn::Error::new_spanned(ty, "Vec/VecDeque needs an element type")
                })?;
                return Ok(FieldInfo {
                    optional: false,
                    container: 1,
                    leaf_expr: child_leaf(elem, fory.element(), BytePos::ListElement, opts)?,
                    key_expr: None,
                    default_ty: None,
                });
            }
            "HashSet" | "BTreeSet" => {
                let elem = args
                    .first()
                    .ok_or_else(|| syn::Error::new_spanned(ty, "set needs an element type"))?;
                return Ok(FieldInfo {
                    optional: false,
                    container: 2,
                    leaf_expr: child_leaf(elem, fory.element(), BytePos::ListElement, opts)?,
                    key_expr: None,
                    default_ty: None,
                });
            }
            "HashMap" | "BTreeMap" => {
                let key = args
                    .first()
                    .ok_or_else(|| syn::Error::new_spanned(ty, "map needs a key type"))?;
                let val = args
                    .get(1)
                    .ok_or_else(|| syn::Error::new_spanned(ty, "map needs a value type"))?;
                return Ok(FieldInfo {
                    optional: false,
                    container: 3,
                    leaf_expr: child_leaf(val, fory.value(), BytePos::MapValue, opts)?,
                    key_expr: Some(child_leaf(key, fory.key(), BytePos::MapKey, opts)?),
                    default_ty: None,
                });
            }
            _ => {}
        }
    }
    // Scalar or a user type (Ref) — leaf comes from its WireField impl. A scalar
    // default targets this exact type.
    Ok(FieldInfo {
        optional: false,
        container: 0,
        leaf_expr: leaf_of(ty),
        key_expr: None,
        default_ty: Some(ty.clone()),
    })
}

fn leaf_of(ty: &Type) -> TokenStream2 {
    quote!(<#ty as ::aster::rpc::WireField>::leaf())
}

/// The types a field contributes to transitive Fory registration — i.e. the
/// types we recurse into via `WireField::register_payload`. Mirrors
/// `analyze_type`'s container unwrapping: `Option<T>` unwraps to `T`'s children;
/// byte buffers are binary (none); `Vec`/`VecDeque`/sets yield the element;
/// maps yield key + value; everything else is the field type itself. Scalars
/// resolve to the `WireField` no-op, so listing them is harmless.
fn child_types(ty: &Type) -> Vec<Type> {
    if byte_shape(ty).is_some() {
        return Vec::new();
    }
    let not_bytes = |t: &Type| byte_shape(t).is_none();
    if let Some((ident, args)) = path_segment(ty) {
        match ident.as_str() {
            "Option" | "Box" => return args.first().map(child_types).unwrap_or_default(),
            "Vec" | "VecDeque" | "HashSet" | "BTreeSet" => {
                return args.into_iter().take(1).filter(not_bytes).collect();
            }
            "HashMap" | "BTreeMap" => {
                return args.into_iter().take(2).filter(not_bytes).collect();
            }
            _ => {}
        }
    }
    vec![ty.clone()]
}

/// Parse `#[aster(default ...)]` on a field → an expression of type
/// `Option<Vec<u8>>` (None = required, Some = the lowered default bytes).
fn parse_default(attrs: &[syn::Attribute], info: &FieldInfo) -> syn::Result<TokenStream2> {
    let none = quote!(::core::option::Option::<::std::vec::Vec<u8>>::None);
    for attr in attrs {
        if !attr.path().is_ident("aster") {
            continue;
        }
        // Two accepted forms: `#[aster(default)]` (bare) and
        // `#[aster(default = <expr>)]`.
        let mut result: Option<TokenStream2> = None;
        let mut err: Option<syn::Error> = None;
        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident("default") {
                // `wire` etc. handled elsewhere; ignore here.
                if meta.input.peek(syn::Token![=]) {
                    let _: Expr = meta.value()?.parse()?;
                }
                return Ok(());
            }
            if meta.input.peek(syn::Token![=]) {
                // Scalar default with a value.
                let expr: Expr = meta.value()?.parse()?;
                match &info.default_ty {
                    Some(ty) => {
                        let coerced = coerce_default(ty, &expr);
                        result = Some(quote! {
                            ::core::option::Option::Some({
                                let __v: #ty = #coerced;
                                ::aster::rpc::encode_scalar_default(&__v)
                            })
                        });
                    }
                    None => {
                        err = Some(meta.error(
                            "a container/binary field's default must be empty — use bare \
                             #[aster(default)] for an empty container",
                        ));
                    }
                }
            } else {
                // Bare default — only valid for containers (empty list/set/map).
                if info.container == 0 {
                    err = Some(
                        meta.error("a scalar field default needs a value: #[aster(default = ...)]"),
                    );
                } else {
                    result = Some(quote!(::core::option::Option::Some(::std::vec![0u8])));
                }
            }
            Ok(())
        })?;
        if let Some(e) = err {
            return Err(e);
        }
        if let Some(r) = result {
            return Ok(r);
        }
    }
    Ok(none)
}

/// Coerce a default literal/expr to the field's scalar type. `String` needs an
/// explicit conversion from a `&str` literal; other scalars coerce via the
/// `let` type ascription.
fn coerce_default(ty: &Type, expr: &Expr) -> TokenStream2 {
    if let Some((ident, _)) = path_segment(ty) {
        if ident == "String" {
            return quote!(::std::string::String::from(#expr));
        }
    }
    quote!(#expr)
}

/// Return the last path segment ident + its angle-bracketed type args.
fn path_segment(ty: &Type) -> Option<(String, Vec<Type>)> {
    let Type::Path(tp) = ty else {
        return None;
    };
    let seg = tp.path.segments.last()?;
    let ident = seg.ident.to_string();
    let args = match &seg.arguments {
        PathArguments::AngleBracketed(ab) => ab
            .args
            .iter()
            .filter_map(|a| match a {
                GenericArgument::Type(t) => Some(t.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((ident, args))
}

fn is_u8(ty: &Type) -> bool {
    matches!(path_segment(ty), Some((id, _)) if id == "u8")
}

// ── #[aster::service] ─────────────────────────────────────────────────────────

/// `#[aster::service(name = "X", version = N)]` on a trait of unary
/// `async fn(&self, Req) -> aster::Result<Resp>` methods. Generates:
/// - the trait (re-emitted with `#[async_trait]`, `#[rpc]` attrs stripped);
/// - `XServer<T>` implementing `ServiceDispatch` (decode → call → encode);
/// - `XClient` with one async method per RPC over an `RpcConnection`;
/// - `XServer::<T>::contract()` / `XClient::contract()` → the cross-binding
///   `ServiceContract`.
///
/// Per-method `#[rpc(requires = <expr>)]` (a `CapabilityRequirement`) and
/// `#[rpc(idempotent)]` are supported, as are the streaming patterns
/// (`#[rpc(server_stream | client_stream | bidi_stream)]`). Payload types must
/// derive `#[derive(ForyStruct, AsterType)]`.
///
/// A method may take an extra `ctx: &aster::rpc::CallContext` (or owned
/// `CallContext`) parameter, detected by type: the dispatcher passes the
/// call's context (peer, principal, attributes, metadata, deadline,
/// cancellation). It is not part of the wire contract — adding or removing it
/// never changes the contract id — and the generated client omits it.
#[proc_macro_attribute]
pub fn service(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as ServiceArgs);
    let trait_def = parse_macro_input!(item as ItemTrait);
    expand_service(args, trait_def)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

struct ServiceArgs {
    name: String,
    version: i32,
    /// Browser projection codecs, e.g. `codecs = ["json"]`. Each emits a
    /// generated `<Svc>Projection` (gateway transcoder). Default: none (Fory).
    codecs: Vec<String>,
}

impl Parse for ServiceArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name: Option<String> = None;
        let mut version: i32 = 1;
        let mut codecs: Vec<String> = Vec::new();
        let metas = Punctuated::<MetaNameValue, Token![,]>::parse_terminated(input)?;
        for m in metas {
            if m.path.is_ident("name") {
                if let Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) = &m.value
                {
                    let value = s.value();
                    if is_reserved_namespace(&value) && !expanding_inside_aster() {
                        return Err(syn::Error::new(
                            s.span(),
                            format!(
                                "service name `{value}` is reserved: the `aster.*` namespace is \
                                 for baseline services shipped with the framework. Choose a name \
                                 in your own namespace, e.g. `name = \"myapp.MyService\"`",
                            ),
                        ));
                    }
                    name = Some(value);
                }
            } else if m.path.is_ident("version") {
                if let Expr::Lit(ExprLit {
                    lit: Lit::Int(i), ..
                }) = &m.value
                {
                    version = i.base10_parse()?;
                }
            } else if m.path.is_ident("codecs") {
                // codecs = ["json", ...] — an array of string literals.
                if let Expr::Array(arr) = &m.value {
                    for elem in &arr.elems {
                        if let Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }) = elem
                        {
                            codecs.push(s.value());
                        } else {
                            return Err(syn::Error::new_spanned(
                                elem,
                                "codecs entries must be string literals, e.g. \"json\"",
                            ));
                        }
                    }
                } else {
                    return Err(syn::Error::new_spanned(
                        &m.value,
                        "codecs must be an array, e.g. codecs = [\"json\"]",
                    ));
                }
            }
        }
        Ok(ServiceArgs {
            name: name.ok_or_else(|| {
                syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "#[aster::service] requires name = \"...\"",
                )
            })?,
            version,
            codecs,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Pattern {
    Unary,
    ServerStream,
    ClientStream,
    BidiStream,
}

struct MethodSpec {
    ident: Ident,
    name: String,
    pattern: Pattern,
    /// Request payload type (the element type for client-stream / bidi inputs).
    req_ty: Type,
    /// Response payload type (the element type for server-stream / bidi outputs).
    resp_ty: Type,
    requires: Option<Expr>,
    idempotent: bool,
    /// Unary method with no request arg — the request is the implicit `Empty`.
    no_request: bool,
    /// The optional `CallContext` parameter: its position among the typed
    /// (non-`self`) arguments, and whether it is taken by reference.
    ctx: Option<CtxParam>,
}

#[derive(Clone, Copy)]
struct CtxParam {
    pos: usize,
    by_ref: bool,
}

/// Classify a handler argument type as the framework-injected call context:
/// `&CallContext` (by reference) or `CallContext` (owned), matched on the last
/// path segment so `aster::rpc::CallContext` and an imported `CallContext`
/// both work. `None` for any other type.
fn ctx_param_kind(ty: &Type) -> syn::Result<Option<bool>> {
    let is_ctx = |t: &Type| matches!(path_segment(t), Some((ident, args)) if ident == "CallContext" && args.is_empty());
    match ty {
        Type::Reference(r) if is_ctx(&r.elem) => {
            if r.mutability.is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "take the call context as `&CallContext` or `CallContext`, not `&mut`",
                ));
            }
            Ok(Some(true))
        }
        Type::Path(_) if is_ctx(ty) => Ok(Some(false)),
        _ => Ok(None),
    }
}

fn expand_service(args: ServiceArgs, mut trait_def: ItemTrait) -> syn::Result<TokenStream2> {
    let svc_ident = trait_def.ident.clone();
    let svc_name = args.name;
    let svc_version = args.version;

    let mut specs: Vec<MethodSpec> = Vec::new();
    for item in &mut trait_def.items {
        if let TraitItem::Fn(m) = item {
            specs.push(parse_method(m)?);
        }
    }

    let server_ident = format_ident!("{}Server", svc_ident);
    let client_ident = format_ident!("{}Client", svc_ident);

    // Unique payload root types (each method's request/response). Every root
    // gets its OWN Fory runtime holding the root plus its transitive field
    // tree (nested structs, `Vec<UserStruct>`, map values, …) via
    // `WireField::register_payload`. One runtime per root is what keeps Fory's
    // per-crate `fory_type_index()` scheme collision-safe: two roots whose
    // types come from different crates (e.g. a user payload and the implicit
    // aster `Empty`) may share an index and so must not share a runtime.
    // Serialization is unaffected — every payload frame is a standalone Fory
    // document, so which runtime encodes it only depends on what's registered.
    let mut roots: Vec<String> = Vec::new();
    let mut fory_exprs: Vec<TokenStream2> = Vec::new();
    for s in &specs {
        for ty in [&s.req_ty, &s.resp_ty] {
            let key = quote!(#ty).to_string();
            if !roots.contains(&key) {
                roots.push(key);
                fory_exprs.push(quote! {
                    ::std::sync::Arc::new({
                        let mut __reg = ::aster::rpc::PayloadRegistry::new();
                        <#ty as ::aster::rpc::WireField>::register_payload(&mut __reg);
                        __reg.into_fory()
                    })
                });
            }
        }
    }
    // Contract type collection: the same roots, walked transitively into
    // the `TypeDef`s a published contract collection carries.
    let collect_roots: Vec<TokenStream2> = specs
        .iter()
        .flat_map(|s| [&s.req_ty, &s.resp_ty])
        .map(|ty| quote!(<#ty as ::aster::rpc::WireField>::collect_type_defs(&mut __set);))
        .collect();
    let build_fory = quote! {
        ::std::vec![ #(#fory_exprs),* ]
    };
    // Compile-time root-type → runtime index lookup for the generated call
    // sites (`self.fory[idx]`).
    let root_idx = |ty: &Type| -> usize {
        let key = quote!(#ty).to_string();
        roots
            .iter()
            .position(|k| *k == key)
            .expect("payload root type not in registration list")
    };

    // ServiceContract expression.
    let method_defs: Vec<TokenStream2> = specs
        .iter()
        .map(|s| {
            let name = &s.name;
            let req = &s.req_ty;
            let resp = &s.resp_ty;
            let idem = s.idempotent;
            let pattern = pattern_token(s.pattern);
            let requires = match &s.requires {
                Some(e) => quote!(::core::option::Option::Some(#e)),
                None => quote!(::core::option::Option::None),
            };
            quote! {
                ::aster::rpc::MethodDef {
                    name: #name.to_string(),
                    pattern: #pattern,
                    request_type: <#req as ::aster::rpc::AsterType>::aster_type_hash_hex(),
                    response_type: <#resp as ::aster::rpc::AsterType>::aster_type_hash_hex(),
                    idempotent: #idem,
                    default_timeout: 0.0,
                    requires: #requires,
                }
            }
        })
        .collect();
    let contract_expr = quote! {
        ::aster::rpc::ServiceContract {
            name: #svc_name.to_string(),
            version: #svc_version,
            methods: ::std::vec![ #(#method_defs),* ],
            serialization_modes: ::std::vec!["xlang".to_string()],
            scoped: ::aster::rpc::ScopeKind::Shared,
            requires: ::core::option::Option::None,
            producer_language: ::std::string::String::new(),
        }
    };

    // ServiceDispatch pieces.
    let method_name_lits: Vec<&String> = specs.iter().map(|s| &s.name).collect();
    let method_requires_arms: Vec<TokenStream2> = specs
        .iter()
        .filter_map(|s| {
            s.requires.as_ref().map(|e| {
                let name = &s.name;
                quote!(#name => ::core::option::Option::Some(#e),)
            })
        })
        .collect();
    let dispatch_arms: Vec<TokenStream2> = specs
        .iter()
        .map(|s| dispatch_arm(s, root_idx(&s.req_ty), root_idx(&s.resp_ty)))
        .collect();

    // Client methods.
    let client_methods: Vec<TokenStream2> = specs
        .iter()
        .map(|s| {
            client_method(
                s,
                &svc_name,
                svc_version,
                root_idx(&s.req_ty),
                root_idx(&s.resp_ty),
            )
        })
        .collect();

    // Optional browser JSON projection (gateway). Generated only when the
    // service opts in via `codecs = ["json"]`; references `::serde_json` and
    // requires the payload types to derive `serde::{Serialize, Deserialize}`.
    let projection_code = if args.codecs.iter().any(|c| c == "json") {
        let projection_ident = format_ident!("{}Projection", svc_ident);
        let pattern_arms: Vec<TokenStream2> = specs
            .iter()
            .map(|s| {
                let name = &s.name;
                let pat = pattern_token(s.pattern);
                quote!(#name => ::core::option::Option::Some(#pat),)
            })
            .collect();
        let req_arms: Vec<TokenStream2> = specs
            .iter()
            .map(|s| {
                let name = &s.name;
                let req = &s.req_ty;
                let idx = root_idx(&s.req_ty);
                quote!(#name => {
                    let __v: #req = ::serde_json::from_slice(body)
                        .map_err(|e| ::std::string::ToString::to_string(&e))?;
                    self.fory[#idx].serialize(&__v).map_err(|e| ::std::string::ToString::to_string(&e))
                })
            })
            .collect();
        let resp_arms: Vec<TokenStream2> = specs
            .iter()
            .map(|s| {
                let name = &s.name;
                let resp = &s.resp_ty;
                let idx = root_idx(&s.resp_ty);
                quote!(#name => {
                    let __v: #resp = self.fory[#idx].deserialize(fory)
                        .map_err(|e| ::std::string::ToString::to_string(&e))?;
                    ::serde_json::to_vec(&__v).map_err(|e| ::std::string::ToString::to_string(&e))
                })
            })
            .collect();
        let projection_ident_str = projection_ident.to_string();
        quote! {
            #[doc = "Generated browser JSON projection (gateway) for this service."]
            #[doc = ""]
            #[doc = "Register with `aster::rpc::ProjectionRegistry::register` and pass to"]
            #[doc = "`aster_transport_salvo::router_with`. Transcodes JSON⇄Fory at the HTTP"]
            #[doc = "edge; the dispatcher still runs ordinary Fory."]
            pub struct #projection_ident {
                fory: ::std::vec::Vec<::std::sync::Arc<::aster::rpc::Fory>>,
            }
            impl #projection_ident {
                pub fn new() -> Self {
                    Self { fory: #build_fory }
                }
            }
            impl ::core::default::Default for #projection_ident {
                fn default() -> Self { Self::new() }
            }
            impl ::aster::rpc::Projection for #projection_ident {
                fn service(&self) -> &str { #svc_name }
                fn pattern(&self, method: &str)
                    -> ::core::option::Option<::aster::rpc::MethodPattern>
                {
                    match method {
                        #(#pattern_arms)*
                        _ => ::core::option::Option::None,
                    }
                }
                fn request_to_fory(&self, method: &str, _media: &str, body: &[u8])
                    -> ::core::result::Result<::std::vec::Vec<u8>, ::std::string::String>
                {
                    match method {
                        #(#req_arms)*
                        _ => ::core::result::Result::Err(
                            ::std::format!("{}: unknown method '{}'", #projection_ident_str, method)),
                    }
                }
                fn response_from_fory(&self, method: &str, _media: &str, fory: &[u8])
                    -> ::core::result::Result<::std::vec::Vec<u8>, ::std::string::String>
                {
                    match method {
                        #(#resp_arms)*
                        _ => ::core::result::Result::Err(
                            ::std::format!("{}: unknown method '{}'", #projection_ident_str, method)),
                    }
                }
            }
        }
    } else {
        quote!()
    };

    Ok(quote! {
        #[::aster::rpc::async_trait]
        #trait_def

        /// Server adapter: wrap a trait impl and register it with an `rpc::Server`.
        pub struct #server_ident<T> {
            inner: T,
            fory: ::std::vec::Vec<::std::sync::Arc<::aster::rpc::Fory>>,
        }

        impl<T: #svc_ident + ::core::marker::Send + ::core::marker::Sync + 'static> #server_ident<T> {
            pub fn new(inner: T) -> Self {
                Self { inner, fory: #build_fory }
            }
            /// The cross-binding `ServiceContract` for this service.
            pub fn contract() -> ::aster::rpc::ServiceContract {
                #contract_expr
            }
        }

        #[::aster::rpc::async_trait]
        impl<T: #svc_ident + ::core::marker::Send + ::core::marker::Sync + 'static>
            ::aster::rpc::ServiceDispatch for #server_ident<T>
        {
            fn name(&self) -> &str { #svc_name }
            fn version(&self) -> i32 { #svc_version }
            fn methods(&self) -> &[&str] { &[ #(#method_name_lits),* ] }
            fn contract_artifacts(&self)
                -> ::core::option::Option<::aster::rpc::ContractArtifacts>
            {
                let mut __set = ::aster::rpc::TypeDefSet::new();
                #(#collect_roots)*
                ::core::option::Option::Some(::aster::rpc::ContractArtifacts {
                    contract: #contract_expr,
                    type_defs: __set.into_defs(),
                })
            }
            fn method_requires(&self, method: &str)
                -> ::core::option::Option<::aster::rpc::CapabilityRequirement>
            {
                match method {
                    #(#method_requires_arms)*
                    _ => ::core::option::Option::None,
                }
            }
            async fn dispatch(&self, method: &str, mut call: ::aster::rpc::Call) {
                match method {
                    #(#dispatch_arms)*
                    other => {
                        let _ = call.finish(&::aster::rpc::RpcStatus::error(
                            ::aster::rpc::StatusCode::Unimplemented, other,
                        )).await;
                    }
                }
            }
        }

        /// Client stub over an `RpcConnection`.
        pub struct #client_ident {
            conn: ::aster::rpc::RpcConnection,
            fory: ::std::vec::Vec<::std::sync::Arc<::aster::rpc::Fory>>,
        }

        impl #client_ident {
            pub fn new(conn: ::aster::rpc::RpcConnection) -> Self {
                Self { conn, fory: #build_fory }
            }
            /// The cross-binding `ServiceContract` for this service.
            pub fn contract() -> ::aster::rpc::ServiceContract {
                #contract_expr
            }
            #(#client_methods)*
        }

        #projection_code
    })
}

/// Parse one trait method into a [`MethodSpec`], extracting the request /
/// response payload types per the method's pattern, and strip its `#[rpc]`
/// attribute(s). Recognised signatures:
/// - unary: `async fn(&self, Req) -> Result<Resp>`
/// - `#[rpc(server_stream)]`: `async fn(&self, Req, ResponseSink<Resp>) -> Result<()>`
/// - `#[rpc(client_stream)]`: `async fn(&self, RequestStream<Req>) -> Result<Resp>`
/// - `#[rpc(bidi_stream)]`: `async fn(&self, RequestStream<Req>, ResponseSink<Resp>) -> Result<()>`
///
/// Any of these may add one `ctx: &CallContext` (or owned `ctx: CallContext`)
/// parameter, conventionally last; it is detected by type, receives the
/// call's `aster::rpc::CallContext`, and is excluded from the contract.
fn parse_method(m: &mut syn::TraitItemFn) -> syn::Result<MethodSpec> {
    let ident = m.sig.ident.clone();

    if m.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &m.sig,
            "#[aster::service] methods must be `async fn`",
        ));
    }

    // Parse + strip #[rpc(...)]: requires / idempotent / streaming pattern.
    let mut requires: Option<Expr> = None;
    let mut idempotent = false;
    let mut pattern = Pattern::Unary;
    let mut name_override: Option<String> = None;
    for attr in &m.attrs {
        if !attr.path().is_ident("rpc") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("requires") {
                requires = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("name") {
                // Wire method name override (for cross-binding parity with
                // camelCase peers, e.g. `#[rpc(name = "getStatus")]`). Defaults
                // to the Rust fn name.
                name_override = Some(meta.value()?.parse::<syn::LitStr>()?.value());
            } else if meta.path.is_ident("idempotent") {
                idempotent = true;
            } else if meta.path.is_ident("server_stream") {
                pattern = Pattern::ServerStream;
            } else if meta.path.is_ident("client_stream") {
                pattern = Pattern::ClientStream;
            } else if meta.path.is_ident("bidi_stream") {
                pattern = Pattern::BidiStream;
            } else {
                return Err(meta.error("unknown #[rpc(...)] key"));
            }
            Ok(())
        })?;
    }
    m.attrs.retain(|a| !a.path().is_ident("rpc"));
    let name = name_override.unwrap_or_else(|| ident.to_string());

    // Receiver + the typed (non-self) arguments.
    let mut inputs = m.sig.inputs.iter();
    if !matches!(inputs.next(), Some(FnArg::Receiver(_))) {
        return Err(syn::Error::new_spanned(
            &m.sig,
            "#[aster::service] methods must take &self",
        ));
    }
    let typed: Vec<&Type> = inputs
        .filter_map(|a| match a {
            FnArg::Typed(pt) => Some(&*pt.ty),
            FnArg::Receiver(_) => None,
        })
        .collect();
    // The optional call-context parameter is framework injection, not a wire
    // field: set it aside before reading the payload types, so it never
    // reaches the contract (and never changes the contract id).
    let mut ctx: Option<CtxParam> = None;
    let mut args: Vec<&Type> = Vec::with_capacity(typed.len());
    for (pos, ty) in typed.into_iter().enumerate() {
        match ctx_param_kind(ty)? {
            Some(by_ref) => {
                if ctx.is_some() {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "at most one `CallContext` parameter per method",
                    ));
                }
                ctx = Some(CtxParam { pos, by_ref });
            }
            None => args.push(ty),
        }
    }

    let result_ok = |sig: &syn::Signature| -> syn::Result<Type> {
        match &sig.output {
            ReturnType::Type(_, ty) => result_ok_type(ty)
                .ok_or_else(|| syn::Error::new_spanned(ty, "method must return `Result<...>`")),
            ReturnType::Default => Err(syn::Error::new_spanned(
                sig,
                "method must return `Result<...>`",
            )),
        }
    };
    let arg_err = |msg: &str| syn::Error::new_spanned(&m.sig, msg.to_string());

    // A unary method may take no request arg (`async fn foo(&self) -> Result<T>`):
    // the request is the implicit `Empty` (Aster's call protocol always carries
    // one request frame; the server ignores it).
    let mut no_request = false;
    let (req_ty, resp_ty) = match pattern {
        Pattern::Unary => match args.len() {
            0 => {
                no_request = true;
                (syn::parse_quote!(::aster::rpc::Empty), result_ok(&m.sig)?)
            }
            1 => (args[0].clone(), result_ok(&m.sig)?),
            _ => {
                return Err(arg_err(
                    "unary: `async fn(&self, req: Req) -> Result<Resp>` or `async fn(&self) -> Result<Resp>`",
                ))
            }
        },
        Pattern::ServerStream => {
            if args.len() != 2 {
                return Err(arg_err(
                    "server-stream: `async fn(&self, req: Req, out: ResponseSink<Resp>) -> Result<()>`",
                ));
            }
            let resp = inner_generic(args[1], "ResponseSink")
                .ok_or_else(|| syn::Error::new_spanned(args[1], "expected `ResponseSink<Resp>`"))?;
            (args[0].clone(), resp)
        }
        Pattern::ClientStream => {
            if args.len() != 1 {
                return Err(arg_err(
                    "client-stream: `async fn(&self, reqs: RequestStream<Req>) -> Result<Resp>`",
                ));
            }
            let req = inner_generic(args[0], "RequestStream")
                .ok_or_else(|| syn::Error::new_spanned(args[0], "expected `RequestStream<Req>`"))?;
            (req, result_ok(&m.sig)?)
        }
        Pattern::BidiStream => {
            if args.len() != 2 {
                return Err(arg_err(
                    "bidi: `async fn(&self, reqs: RequestStream<Req>, out: ResponseSink<Resp>) -> Result<()>`",
                ));
            }
            let req = inner_generic(args[0], "RequestStream")
                .ok_or_else(|| syn::Error::new_spanned(args[0], "expected `RequestStream<Req>`"))?;
            let resp = inner_generic(args[1], "ResponseSink")
                .ok_or_else(|| syn::Error::new_spanned(args[1], "expected `ResponseSink<Resp>`"))?;
            (req, resp)
        }
    };

    Ok(MethodSpec {
        ident,
        name,
        pattern,
        req_ty,
        resp_ty,
        requires,
        idempotent,
        no_request,
        ctx,
    })
}

/// If `ty` is `…Result<Ok, …>`, return the `Ok` type.
fn result_ok_type(ty: &Type) -> Option<Type> {
    let (ident, args) = path_segment(ty)?;
    if ident == "Result" {
        args.into_iter().next()
    } else {
        None
    }
}

/// If `ty`'s last path segment is `wrapper<Inner, …>`, return `Inner` (e.g.
/// `RequestStream<Req>` → `Req`).
fn inner_generic(ty: &Type, wrapper: &str) -> Option<Type> {
    let (ident, args) = path_segment(ty)?;
    if ident == wrapper {
        args.into_iter().next()
    } else {
        None
    }
}

fn pattern_token(p: Pattern) -> TokenStream2 {
    match p {
        Pattern::Unary => quote!(::aster::rpc::MethodPattern::Unary),
        Pattern::ServerStream => quote!(::aster::rpc::MethodPattern::ServerStream),
        Pattern::ClientStream => quote!(::aster::rpc::MethodPattern::ClientStream),
        Pattern::BidiStream => quote!(::aster::rpc::MethodPattern::BidiStream),
    }
}

/// Decode the (single) request payload, bailing with INVALID_ARGUMENT on failure.
fn decode_one_request(req: &Type, req_idx: usize) -> TokenStream2 {
    quote! {
        let __raw = call.recv_request().await.unwrap_or_default();
        let __req: #req = match self.fory[#req_idx].deserialize(&__raw) {
            ::core::result::Result::Ok(r) => r,
            ::core::result::Result::Err(e) => {
                let _ = call.finish(&::aster::rpc::RpcStatus::error(
                    ::aster::rpc::StatusCode::InvalidArgument,
                    ::std::format!("request decode: {e}"),
                )).await;
                return;
            }
        };
    }
}

/// Encode `__resp` and complete the unary/client-stream call, or finish with an
/// INTERNAL error if encoding fails.
fn respond_one(resp_idx: usize) -> TokenStream2 {
    quote! {
        match self.fory[#resp_idx].serialize(&__resp) {
            ::core::result::Result::Ok(__bytes) => {
                let _ = call.respond(__bytes, &::aster::rpc::RpcStatus::ok()).await;
            }
            ::core::result::Result::Err(e) => {
                let _ = call.finish(&::aster::rpc::RpcStatus::error(
                    ::aster::rpc::StatusCode::Internal,
                    ::std::format!("response encode: {e}"),
                )).await;
            }
        }
    }
}

/// The `match` arm in the generated `ServiceDispatch::dispatch` for one method.
fn dispatch_arm(s: &MethodSpec, req_idx: usize, resp_idx: usize) -> TokenStream2 {
    let name = &s.name;
    let ident = &s.ident;
    let req = &s.req_ty;
    let resp = &s.resp_ty;
    // The handler call with its payload arguments, plus the call context at
    // the position the method declared it.
    let invoke = |payload: Vec<TokenStream2>| -> TokenStream2 {
        let mut args = payload;
        if let Some(c) = s.ctx {
            let ctx = if c.by_ref {
                quote!(&__ctx)
            } else {
                quote!(::core::clone::Clone::clone(&__ctx))
            };
            args.insert(c.pos.min(args.len()), ctx);
        }
        quote!(self.inner.#ident(#(#args),*))
    };
    let bind_ctx = if s.ctx.is_some() {
        quote!(let __ctx = ::core::clone::Clone::clone(call.context());)
    } else {
        quote!()
    };
    match s.pattern {
        Pattern::Unary => {
            let respond = respond_one(resp_idx);
            // No-request unary: consume the (ignored) request frame and call the
            // handler with no argument; otherwise decode the request normally.
            let (decode, invoke) = if s.no_request {
                (
                    quote! { let _ = call.recv_request().await; },
                    invoke(vec![]),
                )
            } else {
                (
                    decode_one_request(req, req_idx),
                    invoke(vec![quote!(__req)]),
                )
            };
            quote! {
                #name => {
                    #decode
                    #bind_ctx
                    match #invoke.await {
                        ::core::result::Result::Ok(__resp) => { #respond }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e)).await;
                        }
                    }
                }
            }
        }
        Pattern::ServerStream => {
            let decode = decode_one_request(req, req_idx);
            let invoke = invoke(vec![quote!(__req), quote!(__sink)]);
            quote! {
                #name => {
                    #decode
                    #bind_ctx
                    let __sink = call.response_sink::<#resp>(self.fory[#resp_idx].clone());
                    match #invoke.await {
                        ::core::result::Result::Ok(()) => {
                            let _ = call.finish(&::aster::rpc::RpcStatus::ok()).await;
                        }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e)).await;
                        }
                    }
                }
            }
        }
        Pattern::ClientStream => {
            let respond = respond_one(resp_idx);
            let invoke = invoke(vec![quote!(__reqs)]);
            quote! {
                #name => {
                    #bind_ctx
                    let __reqs = call.take_requests::<#req>(self.fory[#req_idx].clone());
                    match #invoke.await {
                        ::core::result::Result::Ok(__resp) => { #respond }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e)).await;
                        }
                    }
                }
            }
        }
        Pattern::BidiStream => {
            let invoke = invoke(vec![quote!(__reqs), quote!(__sink)]);
            quote! {
                #name => {
                    #bind_ctx
                    let __reqs = call.take_requests::<#req>(self.fory[#req_idx].clone());
                    let __sink = call.response_sink::<#resp>(self.fory[#resp_idx].clone());
                    match #invoke.await {
                        ::core::result::Result::Ok(()) => {
                            let _ = call.finish(&::aster::rpc::RpcStatus::ok()).await;
                        }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e)).await;
                        }
                    }
                }
            }
        }
    }
}

/// One client-stub method.
fn client_method(
    s: &MethodSpec,
    svc_name: &str,
    svc_version: i32,
    req_idx: usize,
    resp_idx: usize,
) -> TokenStream2 {
    let ident = &s.ident;
    let name = &s.name;
    let req = &s.req_ty;
    let resp = &s.resp_ty;
    let header = quote! {
        ::aster::rpc::StreamHeader {
            service: #svc_name.to_string(),
            method: #name.to_string(),
            version: #svc_version,
            call_id: 0,
            deadline: 0,
            serialization_mode: ::aster::rpc::SerializationMode::Xlang.as_i8(),
            metadata_keys: ::std::vec![],
            metadata_values: ::std::vec![],
            session_id: 0,
        }
    };
    let encode_err = quote! {
        |e| ::aster::Error::InvalidArgument(::std::format!("request encode: {e}"))
    };
    let decode_err = quote! {
        |e| ::aster::Error::InvalidArgument(::std::format!("response decode: {e}"))
    };
    match s.pattern {
        // No-request unary → no `req` parameter; send an implicit `Empty`.
        Pattern::Unary if s.no_request => quote! {
            pub async fn #ident(&self) -> ::aster::Result<#resp> {
                let __bytes = self.fory[#req_idx].serialize(&<#req>::default()).map_err(#encode_err)?;
                let __resp_bytes = self.conn.unary(&#header, __bytes).await?;
                self.fory[#resp_idx].deserialize::<#resp>(&__resp_bytes).map_err(#decode_err)
            }
        },
        Pattern::Unary => quote! {
            pub async fn #ident(&self, req: #req) -> ::aster::Result<#resp> {
                let __bytes = self.fory[#req_idx].serialize(&req).map_err(#encode_err)?;
                let __resp_bytes = self.conn.unary(&#header, __bytes).await?;
                self.fory[#resp_idx].deserialize::<#resp>(&__resp_bytes).map_err(#decode_err)
            }
        },
        Pattern::ServerStream => quote! {
            pub async fn #ident(&self, req: #req)
                -> ::aster::Result<::aster::rpc::MessageStream<#resp>>
            {
                let __bytes = self.fory[#req_idx].serialize(&req).map_err(#encode_err)?;
                let __raw = self.conn.server_stream(&#header, __bytes).await?;
                ::core::result::Result::Ok(::aster::rpc::MessageStream::new(__raw, self.fory[#resp_idx].clone()))
            }
        },
        Pattern::ClientStream => quote! {
            pub async fn #ident(&self, reqs: ::std::vec::Vec<#req>) -> ::aster::Result<#resp> {
                let mut __encoded = ::std::vec::Vec::with_capacity(reqs.len());
                for __r in &reqs {
                    __encoded.push(self.fory[#req_idx].serialize(__r).map_err(#encode_err)?);
                }
                let __resp_bytes = self.conn.client_stream(&#header, __encoded).await?;
                self.fory[#resp_idx].deserialize::<#resp>(&__resp_bytes).map_err(#decode_err)
            }
        },
        Pattern::BidiStream => {
            let stream_ident = format_ident!("{}_streaming", ident);
            quote! {
                pub async fn #ident(&self, reqs: ::std::vec::Vec<#req>)
                    -> ::aster::Result<::aster::rpc::MessageStream<#resp>>
                {
                    let mut __encoded = ::std::vec::Vec::with_capacity(reqs.len());
                    for __r in &reqs {
                        __encoded.push(self.fory[#req_idx].serialize(__r).map_err(#encode_err)?);
                    }
                    let __raw = self.conn.bidi(&#header, __encoded).await?;
                    ::core::result::Result::Ok(::aster::rpc::MessageStream::new(__raw, self.fory[#resp_idx].clone()))
                }

                /// Open this bidi call for **incremental** sending: returns a typed
                /// [`BidiCall`](::aster::rpc::BidiCall) sender plus the response
                /// stream, for interactive use (e.g. a remote shell) where requests
                /// are produced over time rather than known up front.
                pub async fn #stream_ident(&self)
                    -> ::aster::Result<(
                        ::aster::rpc::BidiCall<#req>,
                        ::aster::rpc::MessageStream<#resp>,
                    )>
                {
                    let (__sink, __raw) = self.conn.bidi_open(&#header).await?;
                    ::core::result::Result::Ok((
                        ::aster::rpc::BidiCall::new(__sink, self.fory[#req_idx].clone()),
                        ::aster::rpc::MessageStream::new(__raw, self.fory[#resp_idx].clone()),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{analyze_field, child_types, is_reserved_namespace, parse_method, FieldInfo};
    use syn::parse::Parser;

    /// Analyze one named field written as source, e.g.
    /// `#[fory(bytes)] data: Vec<u8>`.
    fn field(src: &str) -> syn::Result<FieldInfo> {
        let f = syn::Field::parse_named
            .parse_str(src)
            .expect("test field must parse");
        analyze_field(&f)
    }

    fn leaf(info: &FieldInfo) -> String {
        info.leaf_expr.to_string()
    }

    fn err(src: &str) -> String {
        match field(src) {
            Ok(_) => panic!("`{src}` should be rejected"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn unannotated_vec_u8_is_a_compile_error_naming_the_fix() {
        let msg = err("data: Vec<u8>");
        assert!(msg.contains("#[fory(bytes)]"), "{msg}");
        assert!(msg.contains("#[aster(uint8_list)]"), "{msg}");
        // `Option` is transparent: the same rule applies to the inner type.
        let msg = err("data: Option<Vec<u8>>");
        assert!(msg.contains("#[fory(bytes)]"), "{msg}");
        // Fully qualified paths are recognised too.
        err("data: ::std::vec::Vec<u8>");
    }

    #[test]
    fn uuid_and_ulid_publish_16_byte_leaves() {
        let one = field("#[aster(uuid)] id: [u8; 16]").unwrap();
        assert!(leaf(&one).contains("\"uuid\""), "{}", leaf(&one));
        let opt = field("#[aster(ulid)] id: Option<[u8; 16]>").unwrap();
        assert!(opt.optional);
        assert!(leaf(&opt).contains("\"ulid\""));
        let list = field("#[aster(ulid)] ids: Vec<[u8; 16]>").unwrap();
        assert_eq!(list.container, 1);
        assert!(leaf(&list).contains("\"ulid\""));
        let map = field("#[aster(uuid)] names: BTreeMap<[u8; 16], String>").unwrap();
        assert_eq!(map.container, 3);
        assert!(map
            .key_expr
            .as_ref()
            .unwrap()
            .to_string()
            .contains("\"uuid\""));
        assert!(!leaf(&map).contains("uuid"));
        // Without the attribute it stays binary.
        assert!(leaf(&field("id: [u8; 16]").unwrap()).contains("\"binary\""));

        assert!(err("#[aster(uuid)] id: [u8; 32]").contains("[u8; 16]"));
        assert!(err("#[aster(uuid)] id: String").contains("applies only"));
        assert!(err("#[aster(uuid)] #[aster(ulid)] id: [u8; 16]").contains("not both"));
        assert!(err("#[aster(uuid)] #[fory(bytes)] id: Vec<u8>").contains("[u8; 16]"));
        assert!(err("#[aster(uuid)] #[aster(uint8_list)] id: Vec<u8>").contains("contradict"));
    }

    #[test]
    fn box_is_transparent() {
        let boxed = field("next: Box<Node>").unwrap();
        assert!(!boxed.optional);
        assert_eq!(boxed.container, 0);
        assert_eq!(leaf(&boxed), leaf(&field("next: Node").unwrap()));

        let opt = field("next: Option<Box<Node>>").unwrap();
        assert!(opt.optional);
        assert_eq!(leaf(&opt), leaf(&field("next: Node").unwrap()));

        let list = field("items: Box<Vec<Node>>").unwrap();
        assert_eq!(list.container, 1);

        // Boxed element types recurse into the boxed type for registration.
        let ty: syn::Type = syn::parse_str("Option<Box<Node>>").unwrap();
        let children = child_types(&ty);
        assert_eq!(children.len(), 1);
        assert_eq!(quote::quote!(#(#children)*).to_string(), "Node");
    }

    #[test]
    fn fory_bytes_vec_u8_is_binary() {
        for src in [
            "#[fory(bytes)] data: Vec<u8>",
            "#[fory(id = 3, bytes)] data: Vec<u8>",
            "#[fory(id = 3)] #[fory(bytes)] data: Vec<u8>",
            "#[fory(nullable, bytes)] data: Option<Vec<u8>>",
        ] {
            let info = field(src).unwrap();
            assert_eq!(info.container, 0, "{src}");
            assert!(leaf(&info).contains("\"binary\""), "{src}");
            assert!(info.default_ty.is_none(), "{src}");
        }
        assert!(
            field("#[fory(bytes)] data: Option<Vec<u8>>")
                .unwrap()
                .optional
        );
    }

    #[test]
    fn fixed_u8_array_is_binary_without_attributes() {
        // Fory writes `[u8; N]` as BINARY already.
        let info = field("digest: [u8; 32]").unwrap();
        assert_eq!(info.container, 0);
        assert!(leaf(&info).contains("\"binary\""));
        assert!(child_types(&syn::parse_quote!([u8; 32])).is_empty());
    }

    #[test]
    fn uint8_list_opt_out_publishes_list_of_uint8() {
        for src in [
            "#[aster(uint8_list)] data: Vec<u8>",
            "#[aster(uint8_list)] data: VecDeque<u8>",
            "#[aster(uint8_list)] data: Option<Vec<u8>>",
        ] {
            let info = field(src).unwrap();
            assert_eq!(info.container, 1, "{src}");
            assert!(leaf(&info).contains("\"uint8\""), "{src}");
        }
        assert!(err("#[aster(uint8_list)] #[fory(bytes)] data: Vec<u8>").contains("contradicts"));
        assert!(err("#[aster(uint8_list)] data: String").contains("applies only"));
    }

    #[test]
    fn list_and_map_of_bytes_need_the_nested_fory_config() {
        let info = field("#[fory(list(element(bytes)))] chunks: Vec<Vec<u8>>").unwrap();
        assert_eq!(info.container, 1);
        assert!(leaf(&info).contains("\"binary\""));
        let msg = err("chunks: Vec<Vec<u8>>");
        assert!(msg.contains("#[fory(list(element(bytes)))]"), "{msg}");
        // The opt-out is a top-level field attribute only; nested lists of
        // lists have no contract shape.
        assert!(!msg.contains("uint8_list"), "{msg}");

        let info = field("#[fory(map(value(bytes)))] blobs: HashMap<String, Vec<u8>>").unwrap();
        assert_eq!(info.container, 3);
        assert!(leaf(&info).contains("\"binary\""));
        let msg = err("blobs: BTreeMap<String, Vec<u8>>");
        assert!(msg.contains("#[fory(map(value(bytes)))]"), "{msg}");
        let msg = err("#[fory(map(value(bytes)))] blobs: HashMap<Vec<u8>, Vec<u8>>");
        assert!(msg.contains("#[fory(map(key(bytes)))]"), "{msg}");

        let info = field("hashes: Vec<[u8; 32]>").unwrap();
        assert!(leaf(&info).contains("\"binary\""));

        // Byte buffers never reach `WireField::register_payload`.
        assert!(child_types(&syn::parse_quote!(Vec<Vec<u8>>)).is_empty());
        assert_eq!(
            child_types(&syn::parse_quote!(HashMap<String, Vec<u8>>)).len(),
            1,
            "only the String key"
        );
    }

    #[test]
    fn non_binary_byte_encodings_are_rejected() {
        assert!(err("#[fory(array)] data: Vec<u8>").contains("UINT8_ARRAY"));
        assert!(err("data: VecDeque<u8>").contains("VecDeque<u8>"));
    }

    #[test]
    fn unrelated_fory_and_aster_keys_are_skipped() {
        let info = field(
            "#[fory(id = 7, nullable = false, encoding = varint, with = my::Codec)] \
             #[aster(default = 3)] n: i32",
        )
        .unwrap();
        assert!(info.default_ty.is_some());
        field("#[fory(tuple(element(index = 0, encoding = fixed)))] t: Vec<String>").unwrap();
    }

    #[test]
    fn reserved_namespace_matches_exact_and_prefixed() {
        assert!(is_reserved_namespace("aster"));
        assert!(is_reserved_namespace("aster.ops.NodeInfo"));
        assert!(is_reserved_namespace("aster.trust"));
        assert!(is_reserved_namespace("aster/ops"));
    }

    #[test]
    fn reserved_namespace_ignores_lookalikes() {
        assert!(!is_reserved_namespace(""));
        assert!(!is_reserved_namespace("asterix"));
        assert!(!is_reserved_namespace("aster-foo"));
        assert!(!is_reserved_namespace("portalsync"));
        assert!(!is_reserved_namespace("myapp.aster"));
        assert!(!is_reserved_namespace("Aster")); // reservation is case-exact
    }

    fn method(src: &str) -> syn::Result<super::MethodSpec> {
        let mut m: syn::TraitItemFn = syn::parse_str(src).unwrap();
        parse_method(&mut m)
    }

    fn ty(t: &syn::Type) -> String {
        quote::quote!(#t).to_string().replace(' ', "")
    }

    #[test]
    fn call_context_param_is_detected_and_kept_out_of_the_payload() {
        let m =
            method("async fn get(&self, req: Req, ctx: &CallContext) -> Result<Resp>;").unwrap();
        let c = m.ctx.expect("ctx detected");
        assert_eq!((c.pos, c.by_ref), (1, true));
        assert_eq!(
            (ty(&m.req_ty), ty(&m.resp_ty)),
            ("Req".into(), "Resp".into())
        );

        let m =
            method("async fn who(&self, ctx: aster::rpc::CallContext) -> Result<Resp>;").unwrap();
        let c = m.ctx.expect("ctx detected");
        assert_eq!((c.pos, c.by_ref), (0, false));
        assert!(m.no_request);

        let m = method(
            "#[rpc(client_stream)] async fn sum(&self, ctx: &CallContext, r: RequestStream<Req>) -> Result<Resp>;",
        )
        .unwrap();
        assert_eq!(m.ctx.map(|c| c.pos), Some(0));
        assert_eq!(ty(&m.req_ty), "Req");

        let m = method(
            "#[rpc(bidi_stream)] async fn chat(&self, r: RequestStream<Req>, o: ResponseSink<Resp>, ctx: CallContext) -> Result<()>;",
        )
        .unwrap();
        assert_eq!(m.ctx.map(|c| c.pos), Some(2));
        assert_eq!(
            (ty(&m.req_ty), ty(&m.resp_ty)),
            ("Req".into(), "Resp".into())
        );

        let m = method("async fn get(&self, req: Req) -> Result<Resp>;").unwrap();
        assert!(m.ctx.is_none());
    }

    #[test]
    fn call_context_param_misuse_is_a_compile_error() {
        let err = |src: &str| method(src).err().expect("expected an error").to_string();
        assert!(
            err("async fn get(&self, req: Req, ctx: &mut CallContext) -> Result<Resp>;")
                .contains("not `&mut`")
        );
        assert!(err(
            "async fn get(&self, a: &CallContext, req: Req, b: CallContext) -> Result<Resp>;"
        )
        .contains("at most one"));
    }
}
