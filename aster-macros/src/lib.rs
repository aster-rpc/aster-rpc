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
        _ => {
            return Err(syn::Error::new_spanned(
                ident,
                "#[derive(AsterType)] can only be applied to structs",
            ))
        }
    };

    let mut field_exprs: Vec<TokenStream2> = Vec::new();
    // Transitive Fory registration: recurse into each field's element/value/key
    // type. Scalars resolve to the `WireField` no-op default; nested structs
    // register themselves and recurse further.
    let mut register_children: Vec<TokenStream2> = Vec::new();
    for f in fields {
        let fname = f.ident.as_ref().unwrap().to_string();
        let info = analyze_type(&f.ty)?;
        let default_expr = parse_default(&f.attrs, &info)?;
        let leaf = &info.leaf_expr;
        let key = match &info.key_expr {
            Some(k) => quote!(::core::option::Option::Some(#k)),
            None => quote!(::core::option::Option::None),
        };
        let optional = info.optional;
        let container = info.container;
        field_exprs.push(quote! {
            ::aster::rpc::make_field(#fname, #leaf, #optional, #container, #key, #default_expr)
        });
        for child in child_types(&f.ty) {
            register_children.push(quote! {
                <#child as ::aster::rpc::WireField>::register_payload(__fory, __seen);
            });
        }
    }

    Ok(quote! {
        impl ::aster::rpc::AsterType for #ident {
            fn aster_type_def() -> ::aster::rpc::TypeDef {
                ::aster::rpc::message_type_def(
                    #package,
                    #type_name,
                    ::std::vec![ #(#field_exprs),* ],
                )
            }
        }

        impl ::aster::rpc::WireField for #ident {
            fn leaf() -> ::aster::rpc::Leaf {
                ::aster::rpc::Leaf::reference(<Self as ::aster::rpc::AsterType>::aster_type_hash())
            }

            fn register_payload(
                __fory: &mut ::aster::rpc::Fory,
                __seen: &mut ::std::collections::HashSet<::core::any::TypeId>,
            ) {
                if !__seen.insert(::core::any::TypeId::of::<Self>()) {
                    return;
                }
                let __td = <Self as ::aster::rpc::AsterType>::aster_type_def();
                __fory
                    .register_by_name::<Self>(&::std::format!("{}.{}", __td.package, __td.name))
                    .expect("register payload type");
                #(#register_children)*
            }
        }
    })
}

/// Parse `#[aster(wire = "ns/Name")]` → `(package, name)`. Splits on the last
/// `/`. Defaults to `("", <struct ident>)` when absent.
fn parse_wire_tag(attrs: &[syn::Attribute], ident: &str) -> syn::Result<(String, String)> {
    let mut wire: Option<String> = None;
    for attr in attrs {
        if !attr.path().is_ident("aster") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("wire") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                wire = Some(lit.value());
                Ok(())
            } else {
                // Ignore unknown struct-level keys here; field-level keys are
                // parsed elsewhere.
                Err(meta.error("unsupported #[aster(...)] key on a struct"))
            }
        })?;
    }
    match wire {
        Some(tag) => match tag.rsplit_once('/') {
            Some((ns, name)) => Ok((ns.to_string(), name.to_string())),
            None => Ok((String::new(), tag)),
        },
        None => Ok((String::new(), ident.to_string())),
    }
}

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

/// Analyze a field type into (optional, container, leaf, key). `Option<T>` sets
/// optional; `Vec<u8>` is `binary`; `Vec`/`VecDeque`/sets/maps become containers;
/// everything else resolves its leaf via the `WireField` impl.
fn analyze_type(ty: &Type) -> syn::Result<FieldInfo> {
    if let Some((ident, args)) = path_segment(ty) {
        if ident == "Option" {
            if let Some(inner) = args.first() {
                let mut info = analyze_inner(inner)?;
                info.optional = true;
                return Ok(info);
            }
        }
    }
    analyze_inner(ty)
}

fn analyze_inner(ty: &Type) -> syn::Result<FieldInfo> {
    if let Some((ident, args)) = path_segment(ty) {
        match ident.as_str() {
            "Vec" | "VecDeque" => {
                let elem = args.first().ok_or_else(|| {
                    syn::Error::new_spanned(ty, "Vec/VecDeque needs an element type")
                })?;
                if is_u8(elem) {
                    return Ok(FieldInfo {
                        optional: false,
                        container: 0,
                        leaf_expr: quote!(::aster::rpc::Leaf::primitive("binary")),
                        key_expr: None,
                        default_ty: None,
                    });
                }
                return Ok(FieldInfo {
                    optional: false,
                    container: 1,
                    leaf_expr: leaf_of(elem),
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
                    leaf_expr: leaf_of(elem),
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
                    leaf_expr: leaf_of(val),
                    key_expr: Some(leaf_of(key)),
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
/// `Vec<u8>` is binary (none); `Vec`/`VecDeque`/sets yield the element; maps
/// yield key + value; everything else is the field type itself. Scalars resolve
/// to the `WireField` no-op, so listing them is harmless.
fn child_types(ty: &Type) -> Vec<Type> {
    if let Some((ident, args)) = path_segment(ty) {
        match ident.as_str() {
            "Option" => return args.first().map(child_types).unwrap_or_default(),
            "Vec" | "VecDeque" | "HashSet" | "BTreeSet" => {
                return match args.first() {
                    Some(elem) if is_u8(elem) => Vec::new(),
                    Some(elem) => vec![elem.clone()],
                    None => Vec::new(),
                };
            }
            "HashMap" | "BTreeMap" => {
                return args.into_iter().take(2).collect();
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
/// `#[rpc(idempotent)]` are supported. Payload types must derive
/// `#[derive(ForyStruct, AsterType)]`. Streaming methods are not yet supported.
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
                    name = Some(s.value());
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

    // Unique payload types (for Fory registration; avoid double-registering).
    // Each call registers the type *and its transitive dependencies* (nested
    // structs, `Vec<UserStruct>`, map values, …) via `WireField::register_payload`,
    // deduped at runtime by the shared `__seen` set below.
    let mut seen: Vec<String> = Vec::new();
    let mut register_stmts: Vec<TokenStream2> = Vec::new();
    for s in &specs {
        for ty in [&s.req_ty, &s.resp_ty] {
            let key = quote!(#ty).to_string();
            if !seen.contains(&key) {
                seen.push(key);
                register_stmts.push(quote! {
                    <#ty as ::aster::rpc::WireField>::register_payload(&mut __f, &mut __seen);
                });
            }
        }
    }
    let build_fory = quote! {{
        let mut __f = ::aster::rpc::new_payload_fory();
        let mut __seen = ::std::collections::HashSet::new();
        #(#register_stmts)*
        __f
    }};

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
    let dispatch_arms: Vec<TokenStream2> = specs.iter().map(dispatch_arm).collect();

    // Client methods.
    let client_methods: Vec<TokenStream2> = specs
        .iter()
        .map(|s| client_method(s, &svc_name, svc_version))
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
                quote!(#name => {
                    let __v: #req = ::serde_json::from_slice(body)
                        .map_err(|e| ::std::string::ToString::to_string(&e))?;
                    self.fory.serialize(&__v).map_err(|e| ::std::string::ToString::to_string(&e))
                })
            })
            .collect();
        let resp_arms: Vec<TokenStream2> = specs
            .iter()
            .map(|s| {
                let name = &s.name;
                let resp = &s.resp_ty;
                quote!(#name => {
                    let __v: #resp = self.fory.deserialize(fory)
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
                fory: ::std::sync::Arc<::aster::rpc::Fory>,
            }
            impl #projection_ident {
                pub fn new() -> Self {
                    Self { fory: ::std::sync::Arc::new(#build_fory) }
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
            fory: ::std::sync::Arc<::aster::rpc::Fory>,
        }

        impl<T: #svc_ident + ::core::marker::Send + ::core::marker::Sync + 'static> #server_ident<T> {
            pub fn new(inner: T) -> Self {
                Self { inner, fory: ::std::sync::Arc::new(#build_fory) }
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
                        ));
                    }
                }
            }
        }

        /// Client stub over an `RpcConnection`.
        pub struct #client_ident {
            conn: ::aster::rpc::RpcConnection,
            fory: ::std::sync::Arc<::aster::rpc::Fory>,
        }

        impl #client_ident {
            pub fn new(conn: ::aster::rpc::RpcConnection) -> Self {
                Self { conn, fory: ::std::sync::Arc::new(#build_fory) }
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
    let args: Vec<&Type> = inputs
        .filter_map(|a| match a {
            FnArg::Typed(pt) => Some(&*pt.ty),
            FnArg::Receiver(_) => None,
        })
        .collect();

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
fn decode_one_request(req: &Type) -> TokenStream2 {
    quote! {
        let __raw = call.recv_request().await.unwrap_or_default();
        let __req: #req = match self.fory.deserialize(&__raw) {
            ::core::result::Result::Ok(r) => r,
            ::core::result::Result::Err(e) => {
                let _ = call.finish(&::aster::rpc::RpcStatus::error(
                    ::aster::rpc::StatusCode::InvalidArgument,
                    ::std::format!("request decode: {e}"),
                ));
                return;
            }
        };
    }
}

/// Encode `__resp` and complete the unary/client-stream call, or finish with an
/// INTERNAL error if encoding fails.
fn respond_one() -> TokenStream2 {
    quote! {
        match self.fory.serialize(&__resp) {
            ::core::result::Result::Ok(__bytes) => {
                let _ = call.respond(__bytes, &::aster::rpc::RpcStatus::ok());
            }
            ::core::result::Result::Err(e) => {
                let _ = call.finish(&::aster::rpc::RpcStatus::error(
                    ::aster::rpc::StatusCode::Internal,
                    ::std::format!("response encode: {e}"),
                ));
            }
        }
    }
}

/// The `match` arm in the generated `ServiceDispatch::dispatch` for one method.
fn dispatch_arm(s: &MethodSpec) -> TokenStream2 {
    let name = &s.name;
    let ident = &s.ident;
    let req = &s.req_ty;
    let resp = &s.resp_ty;
    match s.pattern {
        Pattern::Unary => {
            let respond = respond_one();
            // No-request unary: consume the (ignored) request frame and call the
            // handler with no argument; otherwise decode the request normally.
            let (decode, invoke) = if s.no_request {
                (
                    quote! { let _ = call.recv_request().await; },
                    quote! { self.inner.#ident() },
                )
            } else {
                (decode_one_request(req), quote! { self.inner.#ident(__req) })
            };
            quote! {
                #name => {
                    #decode
                    match #invoke.await {
                        ::core::result::Result::Ok(__resp) => { #respond }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e));
                        }
                    }
                }
            }
        }
        Pattern::ServerStream => {
            let decode = decode_one_request(req);
            quote! {
                #name => {
                    #decode
                    let __sink = call.response_sink::<#resp>(self.fory.clone());
                    match self.inner.#ident(__req, __sink).await {
                        ::core::result::Result::Ok(()) => {
                            let _ = call.finish(&::aster::rpc::RpcStatus::ok());
                        }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e));
                        }
                    }
                }
            }
        }
        Pattern::ClientStream => {
            let respond = respond_one();
            quote! {
                #name => {
                    let __reqs = call.take_requests::<#req>(self.fory.clone());
                    match self.inner.#ident(__reqs).await {
                        ::core::result::Result::Ok(__resp) => { #respond }
                        ::core::result::Result::Err(__e) => {
                            let _ = call.finish(&::aster::rpc::status_from_error(&__e));
                        }
                    }
                }
            }
        }
        Pattern::BidiStream => quote! {
            #name => {
                let __reqs = call.take_requests::<#req>(self.fory.clone());
                let __sink = call.response_sink::<#resp>(self.fory.clone());
                match self.inner.#ident(__reqs, __sink).await {
                    ::core::result::Result::Ok(()) => {
                        let _ = call.finish(&::aster::rpc::RpcStatus::ok());
                    }
                    ::core::result::Result::Err(__e) => {
                        let _ = call.finish(&::aster::rpc::status_from_error(&__e));
                    }
                }
            }
        },
    }
}

/// One client-stub method.
fn client_method(s: &MethodSpec, svc_name: &str, svc_version: i32) -> TokenStream2 {
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
                let __bytes = self.fory.serialize(&<#req>::default()).map_err(#encode_err)?;
                let __resp_bytes = self.conn.unary(&#header, __bytes).await?;
                self.fory.deserialize::<#resp>(&__resp_bytes).map_err(#decode_err)
            }
        },
        Pattern::Unary => quote! {
            pub async fn #ident(&self, req: #req) -> ::aster::Result<#resp> {
                let __bytes = self.fory.serialize(&req).map_err(#encode_err)?;
                let __resp_bytes = self.conn.unary(&#header, __bytes).await?;
                self.fory.deserialize::<#resp>(&__resp_bytes).map_err(#decode_err)
            }
        },
        Pattern::ServerStream => quote! {
            pub async fn #ident(&self, req: #req)
                -> ::aster::Result<::aster::rpc::MessageStream<#resp>>
            {
                let __bytes = self.fory.serialize(&req).map_err(#encode_err)?;
                let __raw = self.conn.server_stream(&#header, __bytes).await?;
                ::core::result::Result::Ok(::aster::rpc::MessageStream::new(__raw, self.fory.clone()))
            }
        },
        Pattern::ClientStream => quote! {
            pub async fn #ident(&self, reqs: ::std::vec::Vec<#req>) -> ::aster::Result<#resp> {
                let mut __encoded = ::std::vec::Vec::with_capacity(reqs.len());
                for __r in &reqs {
                    __encoded.push(self.fory.serialize(__r).map_err(#encode_err)?);
                }
                let __resp_bytes = self.conn.client_stream(&#header, __encoded).await?;
                self.fory.deserialize::<#resp>(&__resp_bytes).map_err(#decode_err)
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
                        __encoded.push(self.fory.serialize(__r).map_err(#encode_err)?);
                    }
                    let __raw = self.conn.bidi(&#header, __encoded).await?;
                    ::core::result::Result::Ok(::aster::rpc::MessageStream::new(__raw, self.fory.clone()))
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
                        ::aster::rpc::BidiCall::new(__sink, self.fory.clone()),
                        ::aster::rpc::MessageStream::new(__raw, self.fory.clone()),
                    ))
                }
            }
        }
    }
}
