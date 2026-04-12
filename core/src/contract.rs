//! Contract identity types, canonical serialization, BLAKE3 hashing, and Tarjan SCC.
//!
//! Translated from `bindings/python/aster/contract/identity.py`.
//! Spec reference: Aster-ContractIdentity.md §11.3

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::canonical::{
    write_bool, write_bytes_field, write_float64, write_list_header, write_optional_absent,
    write_optional_present_prefix, write_string, write_varint, write_zigzag_i32,
};

// ── Enum types (§11.3.3, fixed normative values) ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum TypeKind {
    Primitive = 0,
    Ref = 1,
    SelfRef = 2,
    Any = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKind {
    None = 0,
    List = 1,
    Set = 2,
    Map = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum TypeDefKind {
    Message = 0,
    Enum = 1,
    Union = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum MethodPattern {
    Unary = 0,
    ServerStream = 1,
    ClientStream = 2,
    BidiStream = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Role = 0,
    AnyOf = 1,
    AllOf = 2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    #[default]
    Shared = 0,
    /// One service instance per client connection, all calls multiplexed
    /// onto a single bidirectional QUIC stream. Wire serde produces
    /// "session"; the legacy spelling "stream" is accepted on input via
    /// the deserialize alias.
    #[serde(alias = "stream")]
    Session = 1,
}

// ── Structs ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldDef {
    /// Caller-supplied field ID is accepted on input for backwards compat but
    /// is NOT trusted during canonicalization. `write_type_def` re-derives IDs
    /// from 1-based NFC-name-sorted position per §11.3.2.3 before serializing,
    /// so Java reflection order (and other non-deterministic declaration
    /// orders) cannot affect `contract_id`.
    #[serde(default)]
    pub id: i32,
    pub name: String,
    pub type_kind: TypeKind,
    pub type_primitive: String,
    /// Hex-encoded bytes; decoded to raw bytes for canonical serialization.
    pub type_ref: String,
    pub self_ref_name: String,
    pub optional: bool,
    pub ref_tracked: bool,
    pub container: ContainerKind,
    pub container_key_kind: TypeKind,
    pub container_key_primitive: String,
    /// Hex-encoded bytes; decoded to raw bytes for canonical serialization.
    pub container_key_ref: String,
    /// True if the field has no declared default. Distinct from
    /// "default = zero-value". Part of canonical bytes (field 13). See
    /// §11.3.2.3 defaults rules. Defaults to `true` on serde deserialize
    /// so legacy JSON inputs canonicalize as "all fields required".
    #[serde(default = "default_true")]
    pub required: bool,
    /// Hex-encoded canonical XLANG bytes of the declared default value when
    /// `required = false` and the field's type is scalar. Empty string when
    /// `required = true`. Pinned single-byte sentinel `"00"` for empty
    /// containers (list/set/map). Part of canonical bytes (field 14). See
    /// §11.3.2.3 defaults rules.
    #[serde(default)]
    pub default_value: String,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnumValueDef {
    pub name: String,
    pub value: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnionVariantDef {
    pub name: String,
    pub id: i32,
    /// Hex-encoded bytes; decoded to raw bytes for canonical serialization.
    pub type_ref: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TypeDef {
    pub kind: TypeDefKind,
    pub package: String,
    pub name: String,
    #[serde(default)]
    pub fields: Vec<FieldDef>,
    #[serde(default)]
    pub enum_values: Vec<EnumValueDef>,
    #[serde(default)]
    pub union_variants: Vec<UnionVariantDef>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub kind: CapabilityKind,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MethodDef {
    pub name: String,
    pub pattern: MethodPattern,
    /// Hex-encoded 32-byte BLAKE3 hash.
    pub request_type: String,
    /// Hex-encoded 32-byte BLAKE3 hash.
    pub response_type: String,
    pub idempotent: bool,
    pub default_timeout: f64,
    #[serde(default)]
    pub requires: Option<CapabilityRequirement>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceContract {
    pub name: String,
    pub version: i32,
    #[serde(default)]
    pub methods: Vec<MethodDef>,
    #[serde(default)]
    pub serialization_modes: Vec<String>,
    #[serde(default)]
    pub scoped: ScopeKind,
    #[serde(default)]
    pub requires: Option<CapabilityRequirement>,
    /// REQUIRED when "native" appears in `serialization_modes`; MUST be the
    /// empty string "" otherwise. One of "python" | "typescript" | "java" |
    /// "csharp" | "go" when set. Part of `contract_id` so that two different
    /// native-mode producers produce distinct contract IDs even when their
    /// schemas appear structurally similar. See §11.3.2.3 Serialization
    /// Modes. Canonical byte field 8 (`write_service_contract`).
    #[serde(default)]
    pub producer_language: String,
}

// ── Helper: decode hex string to bytes ───────────────────────────────────────

fn hex_to_bytes(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    hex::decode(s).unwrap_or_default()
}

// ── Canonical serialization functions ────────────────────────────────────────

fn write_field_def(buf: &mut Vec<u8>, fd: &FieldDef) {
    write_zigzag_i32(buf, fd.id); // field 1
    write_string(buf, &fd.name); // field 2
    write_varint(buf, fd.type_kind as u64); // field 3
    write_string(buf, &fd.type_primitive); // field 4
    write_bytes_field(buf, &hex_to_bytes(&fd.type_ref)); // field 5
    write_string(buf, &fd.self_ref_name); // field 6
    write_bool(buf, fd.optional); // field 7
    write_bool(buf, fd.ref_tracked); // field 8
    write_varint(buf, fd.container as u64); // field 9
    write_varint(buf, fd.container_key_kind as u64); // field 10
    write_string(buf, &fd.container_key_primitive); // field 11
    write_bytes_field(buf, &hex_to_bytes(&fd.container_key_ref)); // field 12
    write_bool(buf, fd.required); // field 13
    write_bytes_field(buf, &hex_to_bytes(&fd.default_value)); // field 14
}

/// Compute normative field IDs by NFC-sorting field names and assigning
/// 1-based positions. Per §11.3.2.3, field ID is not developer-managed —
/// it is derived from the field name set alone so that non-deterministic
/// declaration orders (e.g. Java `getDeclaredFields()`) cannot affect
/// `contract_id`.
///
/// Returns a Vec of FieldDef clones with `id` overwritten, sorted by the
/// new ID (which equals NFC-name-sorted order).
fn derive_nfc_sorted_field_ids(fields: &[FieldDef]) -> Vec<FieldDef> {
    let mut sorted: Vec<FieldDef> = fields.to_vec();
    // Sort by NFC-normalized name, Unicode codepoint order — same rule used
    // for method names in ServiceContract. Deterministic and language-
    // independent.
    sorted.sort_by(|a, b| {
        let a_nfc: String = a.name.nfc().collect();
        let b_nfc: String = b.name.nfc().collect();
        let a_cps: Vec<u32> = a_nfc.chars().map(|c| c as u32).collect();
        let b_cps: Vec<u32> = b_nfc.chars().map(|c| c as u32).collect();
        a_cps.cmp(&b_cps)
    });
    // Assign 1-based IDs matching the sorted positions.
    for (i, fd) in sorted.iter_mut().enumerate() {
        fd.id = (i + 1) as i32;
    }
    sorted
}

fn write_enum_value_def(buf: &mut Vec<u8>, ev: &EnumValueDef) {
    write_string(buf, &ev.name); // field 1
    write_zigzag_i32(buf, ev.value); // field 2
}

fn write_union_variant_def(buf: &mut Vec<u8>, uv: &UnionVariantDef) {
    write_string(buf, &uv.name); // field 1
    write_zigzag_i32(buf, uv.id); // field 2
    write_bytes_field(buf, &hex_to_bytes(&uv.type_ref)); // field 3
}

fn write_type_def(buf: &mut Vec<u8>, td: &TypeDef) {
    write_varint(buf, td.kind as u64); // field 1
    write_string(buf, &td.package); // field 2
    write_string(buf, &td.name); // field 3

    // field 4: fields list — re-derive IDs from NFC-name-sorted order per
    // §11.3.2.3 (caller-supplied IDs are ignored). Result is already in
    // ID-ascending == NFC-name order; no secondary sort needed.
    let sorted_fields = derive_nfc_sorted_field_ids(&td.fields);
    write_list_header(buf, sorted_fields.len());
    for fd in &sorted_fields {
        write_field_def(buf, fd);
    }

    // field 5: enum_values list (sorted by value ascending)
    let mut sorted_evs: Vec<&EnumValueDef> = td.enum_values.iter().collect();
    sorted_evs.sort_by_key(|e| e.value);
    write_list_header(buf, sorted_evs.len());
    for ev in &sorted_evs {
        write_enum_value_def(buf, ev);
    }

    // field 6: union_variants list (sorted by id ascending)
    let mut sorted_uvs: Vec<&UnionVariantDef> = td.union_variants.iter().collect();
    sorted_uvs.sort_by_key(|u| u.id);
    write_list_header(buf, sorted_uvs.len());
    for uv in &sorted_uvs {
        write_union_variant_def(buf, uv);
    }
}

fn write_capability_requirement(buf: &mut Vec<u8>, cap: &CapabilityRequirement) {
    write_varint(buf, cap.kind as u64); // field 1

    // field 2: roles list (NFC-normalized, sorted by Unicode codepoint)
    let mut nfc_roles: Vec<String> = cap
        .roles
        .iter()
        .map(|r| r.nfc().collect::<String>())
        .collect();
    nfc_roles.sort_by(|a, b| {
        let a_cps: Vec<u32> = a.chars().map(|c| c as u32).collect();
        let b_cps: Vec<u32> = b.chars().map(|c| c as u32).collect();
        a_cps.cmp(&b_cps)
    });
    write_list_header(buf, nfc_roles.len());
    for role in &nfc_roles {
        write_string(buf, role);
    }
}

pub fn write_method_def(buf: &mut Vec<u8>, md: &MethodDef) {
    write_string(buf, &md.name); // field 1
    write_varint(buf, md.pattern as u64); // field 2
    write_bytes_field(buf, &hex_to_bytes(&md.request_type)); // field 3
    write_bytes_field(buf, &hex_to_bytes(&md.response_type)); // field 4
    write_bool(buf, md.idempotent); // field 5
    write_float64(buf, md.default_timeout); // field 6

    // field 7: requires (optional)
    match &md.requires {
        None => write_optional_absent(buf),
        Some(cap) => {
            write_optional_present_prefix(buf);
            write_capability_requirement(buf, cap);
        }
    }
}

fn write_service_contract(buf: &mut Vec<u8>, sc: &ServiceContract) {
    write_string(buf, &sc.name); // field 1
    write_zigzag_i32(buf, sc.version); // field 2

    // field 3: methods list (sorted by NFC-normalized name, Unicode codepoint order)
    let mut sorted_methods: Vec<&MethodDef> = sc.methods.iter().collect();
    sorted_methods.sort_by(|a, b| {
        let a_nfc: String = a.name.nfc().collect();
        let b_nfc: String = b.name.nfc().collect();
        let a_cps: Vec<u32> = a_nfc.chars().map(|c| c as u32).collect();
        let b_cps: Vec<u32> = b_nfc.chars().map(|c| c as u32).collect();
        a_cps.cmp(&b_cps)
    });
    write_list_header(buf, sorted_methods.len());
    for md in &sorted_methods {
        write_method_def(buf, md);
    }

    // field 4: serialization_modes list
    write_list_header(buf, sc.serialization_modes.len());
    for mode in &sc.serialization_modes {
        write_string(buf, mode);
    }

    // field 5: scoped
    write_varint(buf, sc.scoped as u64);

    // field 6: requires (optional)
    match &sc.requires {
        None => write_optional_absent(buf),
        Some(cap) => {
            write_optional_present_prefix(buf);
            write_capability_requirement(buf, cap);
        }
    }

    // field 7: producer_language — REQUIRED when "native" in serialization_modes,
    // empty string otherwise. See §11.3.2.3 and §11.3.3. (alpn is NOT part of
    // canonical bytes — it is a transport-layer concern pinned via
    // ContractManifest.canonical_encoding, not contract identity.)
    write_string(buf, &sc.producer_language);
}

/// Validate producer_language invariant per §11.3.2.3.
fn validate_producer_language(sc: &ServiceContract) -> Result<()> {
    let has_native = sc.serialization_modes.iter().any(|m| m == "native");
    if has_native && sc.producer_language.is_empty() {
        bail!(
            "ServiceContract declares 'native' in serialization_modes but \
             producer_language is empty; must be one of \
             python|typescript|java|csharp|go (§11.3.2.3)"
        );
    }
    if !has_native && !sc.producer_language.is_empty() {
        bail!(
            "ServiceContract does not declare 'native' in serialization_modes \
             but producer_language = {:?}; must be empty unless native mode \
             is declared (§11.3.2.3)",
            sc.producer_language
        );
    }
    if has_native {
        match sc.producer_language.as_str() {
            "python" | "typescript" | "java" | "csharp" | "go" => {}
            other => bail!(
                "ServiceContract.producer_language = {:?} is not a recognized \
                 language identifier; must be one of python|typescript|java|\
                 csharp|go (§11.3.2.3)",
                other
            ),
        }
    }
    Ok(())
}

// ── Public canonical bytes API ───────────────────────────────────────────────

/// Serialize a TypeDef to canonical bytes.
pub fn canonical_xlang_bytes_type_def(td: &TypeDef) -> Vec<u8> {
    let mut buf = Vec::new();
    write_type_def(&mut buf, td);
    buf
}

/// Serialize a ServiceContract to canonical bytes.
///
/// Does NOT validate `producer_language` — callers are responsible for
/// validation via the JSON entry points (`compute_contract_id_from_json`,
/// `canonical_bytes_from_json`) which enforce the invariant before
/// serializing. The infallible form remains public for in-process Rust
/// callers that construct `ServiceContract` programmatically with known-
/// good values.
pub fn canonical_xlang_bytes_service_contract(sc: &ServiceContract) -> Vec<u8> {
    let mut buf = Vec::new();
    write_service_contract(&mut buf, sc);
    buf
}

/// Serialize a ServiceContract to canonical bytes, validating the
/// `producer_language` invariant per §11.3.2.3. Prefer this over the
/// infallible form when accepting untrusted input.
pub fn canonical_xlang_bytes_service_contract_checked(sc: &ServiceContract) -> Result<Vec<u8>> {
    validate_producer_language(sc)?;
    Ok(canonical_xlang_bytes_service_contract(sc))
}

/// Serialize a MethodDef to canonical bytes.
pub fn canonical_xlang_bytes_method_def(md: &MethodDef) -> Vec<u8> {
    let mut buf = Vec::new();
    write_method_def(&mut buf, md);
    buf
}

/// BLAKE3 hash of canonical bytes -> 32-byte digest.
pub fn compute_type_hash(canonical_bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(canonical_bytes).as_bytes()
}

/// BLAKE3 hash of contract bytes -> 64-char hex string.
pub fn compute_contract_id(contract_bytes: &[u8]) -> String {
    hex::encode(blake3::hash(contract_bytes).as_bytes())
}

/// NFC normalization for identifiers.
pub fn normalize_identifier(s: &str) -> String {
    s.nfc().collect()
}

/// Deserialize a ServiceContract from JSON, compute canonical bytes + BLAKE3 hash.
/// Validates the `producer_language` invariant per §11.3.2.3 before hashing.
/// Returns 64-char hex contract_id.
pub fn compute_contract_id_from_json(json_str: &str) -> Result<String> {
    let sc: ServiceContract = serde_json::from_str(json_str)?;
    let bytes = canonical_xlang_bytes_service_contract_checked(&sc)?;
    Ok(compute_contract_id(&bytes))
}

/// Deserialize from JSON, return canonical bytes. Validates
/// `producer_language` for ServiceContract inputs per §11.3.2.3.
pub fn canonical_bytes_from_json(type_name: &str, json_str: &str) -> Result<Vec<u8>> {
    match type_name {
        "ServiceContract" => {
            let sc: ServiceContract = serde_json::from_str(json_str)?;
            canonical_xlang_bytes_service_contract_checked(&sc)
        }
        "TypeDef" => {
            let td: TypeDef = serde_json::from_str(json_str)?;
            Ok(canonical_xlang_bytes_type_def(&td))
        }
        "MethodDef" => {
            let md: MethodDef = serde_json::from_str(json_str)?;
            Ok(canonical_xlang_bytes_method_def(&md))
        }
        _ => bail!("unknown type: {}", type_name),
    }
}

// ── Tarjan's SCC algorithm ───────────────────────────────────────────────────

/// Find SCCs in reverse topological order (leaves first) using Tarjan's algorithm.
///
/// Processes nodes in sorted order (Unicode codepoint) for determinism.
pub fn tarjan_scc(graph: &HashMap<String, HashSet<String>>) -> Vec<Vec<String>> {
    struct State {
        index_counter: usize,
        stack: Vec<String>,
        lowlink: HashMap<String, usize>,
        index: HashMap<String, usize>,
        on_stack: HashMap<String, bool>,
        sccs: Vec<Vec<String>>,
    }

    fn strongconnect(v: &str, graph: &HashMap<String, HashSet<String>>, state: &mut State) {
        state.index.insert(v.to_string(), state.index_counter);
        state.lowlink.insert(v.to_string(), state.index_counter);
        state.index_counter += 1;
        state.stack.push(v.to_string());
        state.on_stack.insert(v.to_string(), true);

        // Sort successors for determinism
        let empty = HashSet::new();
        let successors = graph.get(v).unwrap_or(&empty);
        let mut sorted_successors: Vec<&String> = successors.iter().collect();
        sorted_successors.sort();

        for w in sorted_successors {
            if !state.index.contains_key(w.as_str()) {
                strongconnect(w, graph, state);
                let lw = state.lowlink[w.as_str()];
                let lv = state.lowlink[v];
                state.lowlink.insert(v.to_string(), lv.min(lw));
            } else if *state.on_stack.get(w.as_str()).unwrap_or(&false) {
                let iw = state.index[w.as_str()];
                let lv = state.lowlink[v];
                state.lowlink.insert(v.to_string(), lv.min(iw));
            }
        }

        if state.lowlink[v] == state.index[v] {
            let mut scc: Vec<String> = Vec::new();
            loop {
                let w = state
                    .stack
                    .pop()
                    .expect("Tarjan SCC invariant: stack must be non-empty when lowlink == index");
                state.on_stack.insert(w.clone(), false);
                scc.push(w.clone());
                if w == v {
                    break;
                }
            }
            state.sccs.push(scc);
        }
    }

    let mut state = State {
        index_counter: 0,
        stack: Vec::new(),
        lowlink: HashMap::new(),
        index: HashMap::new(),
        on_stack: HashMap::new(),
        sccs: Vec::new(),
    };

    // Sorted node list for determinism
    let mut nodes: Vec<&String> = graph.keys().collect();
    nodes.sort();

    for v in nodes {
        if !state.index.contains_key(v.as_str()) {
            strongconnect(v, graph, &mut state);
        }
    }

    state.sccs // Already in reverse topological order from Tarjan's
}

/// DFS spanning tree to identify back-edges within an SCC.
///
/// Edges to already-visited nodes are back-edges. Outgoing edges are sorted
/// for determinism.
pub fn spanning_tree_back_edges(
    start: &str,
    members: &[String],
    graph: &HashMap<String, HashSet<String>>,
) -> HashSet<(String, String)> {
    let member_set: HashSet<&str> = members.iter().map(|s| s.as_str()).collect();
    let mut visited: HashSet<String> = HashSet::new();
    let mut back_edges: HashSet<(String, String)> = HashSet::new();

    fn dfs(
        v: &str,
        member_set: &HashSet<&str>,
        graph: &HashMap<String, HashSet<String>>,
        visited: &mut HashSet<String>,
        back_edges: &mut HashSet<(String, String)>,
    ) {
        visited.insert(v.to_string());
        let empty = HashSet::new();
        let successors = graph.get(v).unwrap_or(&empty);
        // Filter to only SCC members, then sort
        let mut member_successors: Vec<&String> = successors
            .iter()
            .filter(|w| member_set.contains(w.as_str()))
            .collect();
        member_successors.sort();

        for w in member_successors {
            if !visited.contains(w.as_str()) {
                dfs(w, member_set, graph, visited, back_edges);
            } else {
                back_edges.insert((v.to_string(), w.to_string()));
            }
        }
    }

    dfs(start, &member_set, graph, &mut visited, &mut back_edges);
    back_edges
}

/// Processing order for SCC members (leaves-first = reverse post-order on spanning tree).
///
/// Spanning tree edges = all SCC edges minus back-edges.
/// DFS post-order on spanning tree, then reverse.
pub fn scc_processing_order(
    start: &str,
    members: &[String],
    graph: &HashMap<String, HashSet<String>>,
    back_edges: &HashSet<(String, String)>,
) -> Vec<String> {
    let member_set: HashSet<&str> = members.iter().map(|s| s.as_str()).collect();

    // Spanning tree edges = all edges within SCC minus back-edges
    let mut spanning_edges: HashSet<(String, String)> = HashSet::new();
    for fqn in members {
        let empty = HashSet::new();
        let successors = graph.get(fqn).unwrap_or(&empty);
        for target in successors {
            if member_set.contains(target.as_str())
                && !back_edges.contains(&(fqn.clone(), target.clone()))
            {
                spanning_edges.insert((fqn.clone(), target.clone()));
            }
        }
    }

    // DFS post-order on spanning tree
    let mut visited: HashSet<String> = HashSet::new();
    let mut post_order: Vec<String> = Vec::new();

    fn dfs_post(
        v: &str,
        spanning_edges: &HashSet<(String, String)>,
        visited: &mut HashSet<String>,
        post_order: &mut Vec<String>,
    ) {
        visited.insert(v.to_string());
        // Find spanning-tree successors, sort by NFC codepoint
        let mut successors: Vec<String> = spanning_edges
            .iter()
            .filter(|(src, tgt)| src == v && !visited.contains(tgt.as_str()))
            .map(|(_, tgt)| tgt.clone())
            .collect();
        successors.sort_by(|a, b| {
            let a_nfc: String = a.nfc().collect();
            let b_nfc: String = b.nfc().collect();
            let a_cps: Vec<u32> = a_nfc.chars().map(|c| c as u32).collect();
            let b_cps: Vec<u32> = b_nfc.chars().map(|c| c as u32).collect();
            a_cps.cmp(&b_cps)
        });
        for w in successors {
            if !visited.contains(w.as_str()) {
                dfs_post(&w, spanning_edges, visited, post_order);
            }
        }
        post_order.push(v.to_string());
    }

    dfs_post(start, &spanning_edges, &mut visited, &mut post_order);

    // Reverse post-order = processing order (leaves first)
    post_order.reverse();
    post_order
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: construct a FieldDef with common defaults. Caller-supplied
    // `id` is kept for source compatibility but is overwritten during
    // canonicalization per §11.3.2.3 (NFC-name-sorted position).
    #[allow(clippy::too_many_arguments)]
    fn field_def(
        id: i32,
        name: &str,
        type_kind: TypeKind,
        type_primitive: &str,
        type_ref: &str,
        self_ref_name: &str,
        optional: bool,
        ref_tracked: bool,
        container: ContainerKind,
        container_key_kind: TypeKind,
        container_key_primitive: &str,
        container_key_ref: &str,
    ) -> FieldDef {
        FieldDef {
            id,
            name: name.to_string(),
            type_kind,
            type_primitive: type_primitive.to_string(),
            type_ref: type_ref.to_string(),
            self_ref_name: self_ref_name.to_string(),
            optional,
            ref_tracked,
            container,
            container_key_kind,
            container_key_primitive: container_key_primitive.to_string(),
            container_key_ref: container_key_ref.to_string(),
            // Default to required-with-no-default for all test fixtures.
            // Tests that exercise defaults set these fields explicitly.
            required: true,
            default_value: String::new(),
        }
    }

    // Shortcut for a simple primitive field with no container
    fn simple_primitive_field(id: i32, name: &str, primitive: &str) -> FieldDef {
        field_def(
            id,
            name,
            TypeKind::Primitive,
            primitive,
            "",
            "",
            false,
            false,
            ContainerKind::None,
            TypeKind::Primitive,
            "",
            "",
        )
    }

    // Shortcut for a SELF_REF field
    fn self_ref_field(id: i32, name: &str, self_ref_name: &str, optional: bool) -> FieldDef {
        field_def(
            id,
            name,
            TypeKind::SelfRef,
            "",
            "",
            self_ref_name,
            optional,
            false,
            ContainerKind::None,
            TypeKind::Primitive,
            "",
            "",
        )
    }

    // ---- A.2: Minimal ServiceContract (EmptyService) ----

    #[test]
    fn test_a2_empty_service_contract() {
        let sc = ServiceContract {
            name: "EmptyService".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["xlang".to_string()],
            scoped: ScopeKind::Shared,
            requires: None,
            producer_language: String::new(),
        };

        let bytes = canonical_xlang_bytes_service_contract(&sc);
        let bytes_hex = hex::encode(&bytes);
        assert_eq!(
            bytes_hex,
            "32456d7074795365727669636502000c010c16786c616e6700fd02"
        );

        let hash_hex = hex::encode(compute_type_hash(&bytes));
        assert_eq!(
            hash_hex,
            "d016f1c19d536b69c4fb2af96acce700da5c45bd6c4860b6c9ae408b4ca35438"
        );
    }

    // ---- A.3: Minimal TypeDef (enum Color) ----

    #[test]
    fn test_a3_enum_color() {
        let td = TypeDef {
            kind: TypeDefKind::Enum,
            package: "test".to_string(),
            name: "Color".to_string(),
            fields: vec![],
            enum_values: vec![
                EnumValueDef {
                    name: "RED".to_string(),
                    value: 0,
                },
                EnumValueDef {
                    name: "GREEN".to_string(),
                    value: 1,
                },
                EnumValueDef {
                    name: "BLUE".to_string(),
                    value: 2,
                },
            ],
            union_variants: vec![],
        };

        let bytes = canonical_xlang_bytes_type_def(&td);
        let bytes_hex = hex::encode(&bytes);
        assert_eq!(
            bytes_hex,
            "01127465737416436f6c6f72000c030c0e5245440016475245454e0212424c554504000c"
        );

        let hash_hex = hex::encode(compute_type_hash(&bytes));
        assert_eq!(
            hash_hex,
            "bac1586aaa144fa0b565268419da29f18e536f18c7290e4bdf3496919cfa29ce"
        );
    }

    // ---- A.4: TypeDef with REF field (32 bytes 0xAA hash) ----

    #[test]
    fn test_a4_typedef_with_ref() {
        let td = TypeDef {
            kind: TypeDefKind::Message,
            package: "test".to_string(),
            name: "Wrapper".to_string(),
            fields: vec![field_def(
                1,
                "inner",
                TypeKind::Ref,
                "",
                &"aa".repeat(32), // 32 bytes of 0xAA as hex
                "",
                false,
                false,
                ContainerKind::None,
                TypeKind::Primitive,
                "",
                "",
            )],
            enum_values: vec![],
            union_variants: vec![],
        };

        let bytes = canonical_xlang_bytes_type_def(&td);
        let bytes_hex = hex::encode(&bytes);
        assert_eq!(
            bytes_hex,
            "0012746573741e57726170706572010c0216696e6e6572010220aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa020000000002000100000c000c"
        );

        let hash_hex = hex::encode(compute_type_hash(&bytes));
        assert_eq!(
            hash_hex,
            "67396f0456ee178135a0adb73adbf884c57ce0358e2698d3b21a7eb5820d7c4f"
        );
    }

    // ---- A.5: MethodDef with requires present (ANY_OF) ----

    #[test]
    fn test_a5_method_with_requires() {
        let md = MethodDef {
            name: "do_work".to_string(),
            pattern: MethodPattern::Unary,
            request_type: "11".repeat(32),
            response_type: "22".repeat(32),
            idempotent: true,
            default_timeout: 30.0,
            requires: Some(CapabilityRequirement {
                kind: CapabilityKind::AnyOf,
                roles: vec!["Admin".to_string(), "Operator".to_string()],
            }),
        };

        let bytes = canonical_xlang_bytes_method_def(&md);
        let bytes_hex = hex::encode(&bytes);
        assert_eq!(
            bytes_hex,
            "1e646f5f776f726b00201111111111111111111111111111111111111111111111111111111111111111202222222222222222222222222222222222222222222222222222222222222222010000000000003e400001020c1641646d696e224f70657261746f72"
        );

        let hash_hex = hex::encode(compute_type_hash(&bytes));
        assert_eq!(
            hash_hex,
            "c74c82db4d785d96141c6ee621176ce7c1628210802e64b04f9dd0ee4b268fa0"
        );
    }

    // ---- A.6: MethodDef with requires absent ----

    #[test]
    fn test_a6_method_without_requires() {
        let md = MethodDef {
            name: "do_work".to_string(),
            pattern: MethodPattern::Unary,
            request_type: "11".repeat(32),
            response_type: "22".repeat(32),
            idempotent: false,
            default_timeout: 0.0,
            requires: None,
        };

        let bytes = canonical_xlang_bytes_method_def(&md);
        let bytes_hex = hex::encode(&bytes);
        assert_eq!(
            bytes_hex,
            "1e646f5f776f726b00201111111111111111111111111111111111111111111111111111111111111111202222222222222222222222222222222222222222222222222222222222222222000000000000000000fd"
        );

        let hash_hex = hex::encode(compute_type_hash(&bytes));
        assert_eq!(
            hash_hex,
            "de4f82d4139d0897f3ecf93899258bfdaca00681beaea041aad0284a9cd9b569"
        );
    }

    // ---- B.1: Direct self-recursion (TreeNode) ----

    #[test]
    fn test_b1_tree_node_self_recursion() {
        let td = TypeDef {
            kind: TypeDefKind::Message,
            package: "example".to_string(),
            name: "TreeNode".to_string(),
            fields: vec![
                simple_primitive_field(1, "value", "string"),
                self_ref_field(2, "left", "example.TreeNode", true),
                self_ref_field(3, "right", "example.TreeNode", true),
            ],
            enum_values: vec![],
            union_variants: vec![],
        };

        let bytes = canonical_xlang_bytes_type_def(&td);
        let bytes_hex = hex::encode(&bytes);
        assert_eq!(
            bytes_hex,
            "001e6578616d706c6522547265654e6f6465030c02126c656674020200426578616d706c652e547265654e6f6465010000000200010004167269676874020200426578616d706c652e547265654e6f64650100000002000100061676616c7565001a737472696e6700020000000002000100000c000c"
        );

        let hash_hex = hex::encode(compute_type_hash(&bytes));
        assert_eq!(
            hash_hex,
            "cedc95221a13fdeb63b0f78ec7b5b28cc651ec24913560e2ce3025ff702dc6a4"
        );
    }

    // ---- B.2: Mutual recursion (Book hashed first) ----

    #[test]
    fn test_b2_book_mutual_recursion() {
        // Book: written_by is SELF_REF to Author (back-edge)
        let book_td = TypeDef {
            kind: TypeDefKind::Message,
            package: "example".to_string(),
            name: "Book".to_string(),
            fields: vec![
                simple_primitive_field(1, "title", "string"),
                self_ref_field(2, "written_by", "example.Author", false),
            ],
            enum_values: vec![],
            union_variants: vec![],
        };

        let book_bytes = canonical_xlang_bytes_type_def(&book_td);
        let book_hex = hex::encode(&book_bytes);
        assert_eq!(
            book_hex,
            "001e6578616d706c6512426f6f6b020c02167469746c65001a737472696e6700020000000002000100042a7772697474656e5f62790202003a6578616d706c652e417574686f720000000002000100000c000c"
        );

        let book_hash = compute_type_hash(&book_bytes);
        let book_hash_hex = hex::encode(book_hash);
        assert_eq!(
            book_hash_hex,
            "ebbd3f0620150dc212f93cb17cda130c5303bdd7f1f8c7d7c9c6e52cb16677f2"
        );

        // Author: books is list<Book> via REF (using Book's hash)
        let author_td = TypeDef {
            kind: TypeDefKind::Message,
            package: "example".to_string(),
            name: "Author".to_string(),
            fields: vec![
                simple_primitive_field(1, "name", "string"),
                field_def(
                    2,
                    "books",
                    TypeKind::Ref,
                    "",
                    &book_hash_hex,
                    "",
                    false,
                    false,
                    ContainerKind::List,
                    TypeKind::Primitive,
                    "",
                    "",
                ),
            ],
            enum_values: vec![],
            union_variants: vec![],
        };

        let author_bytes = canonical_xlang_bytes_type_def(&author_td);
        let author_hex = hex::encode(&author_bytes);
        assert_eq!(
            author_hex,
            "001e6578616d706c651a417574686f72020c0216626f6f6b73010220ebbd3f0620150dc212f93cb17cda130c5303bdd7f1f8c7d7c9c6e52cb16677f202000001000200010004126e616d65001a737472696e6700020000000002000100000c000c"
        );

        let author_hash_hex = hex::encode(compute_type_hash(&author_bytes));
        assert_eq!(
            author_hash_hex,
            "a852da5eb271c3f4959701356e39126dddafc0b341591d6be8f7c775883218bc"
        );
    }

    // ---- Scope tests ----

    #[test]
    fn test_scope_shared() {
        let sc = ServiceContract {
            name: "ScopeTest".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["xlang".to_string()],
            scoped: ScopeKind::Shared,
            requires: None,
            producer_language: String::new(),
        };
        let bytes = canonical_xlang_bytes_service_contract(&sc);
        assert_eq!(
            hex::encode(&bytes),
            "2653636f70655465737402000c010c16786c616e6700fd02"
        );
        assert_eq!(
            hex::encode(compute_type_hash(&bytes)),
            "8c22cacbc7df48301d17e7bc9cc6e251c58a3cc182f5c7ff3c567929e1eb040b"
        );
    }

    #[test]
    fn test_scope_session() {
        let sc = ServiceContract {
            name: "ScopeTest".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["xlang".to_string()],
            scoped: ScopeKind::Session,
            requires: None,
            producer_language: String::new(),
        };
        let bytes = canonical_xlang_bytes_service_contract(&sc);
        assert_eq!(
            hex::encode(&bytes),
            "2653636f70655465737402000c010c16786c616e6701fd02"
        );
        assert_eq!(
            hex::encode(compute_type_hash(&bytes)),
            "f2d036c871219505a4b7383c837fafdbd7e533d3e3453470c9dc45f96e47d9dc"
        );
    }

    // ---- SCC: no cycles (linear graph) ----

    #[test]
    fn test_scc_no_cycles() {
        let mut graph: HashMap<String, HashSet<String>> = HashMap::new();
        graph.insert("A".to_string(), vec!["B".to_string()].into_iter().collect());
        graph.insert("B".to_string(), vec!["C".to_string()].into_iter().collect());
        graph.insert("C".to_string(), HashSet::new());

        let sccs = tarjan_scc(&graph);
        // Each node is its own SCC; leaves first
        assert_eq!(sccs.len(), 3);
        // C should come first (leaf), then B, then A
        assert_eq!(sccs[0], vec!["C".to_string()]);
        assert_eq!(sccs[1], vec!["B".to_string()]);
        assert_eq!(sccs[2], vec!["A".to_string()]);
    }

    // ---- SCC: self-reference (TreeNode) ----

    #[test]
    fn test_scc_self_reference() {
        let mut graph: HashMap<String, HashSet<String>> = HashMap::new();
        graph.insert(
            "TreeNode".to_string(),
            vec!["TreeNode".to_string()].into_iter().collect(),
        );

        let sccs = tarjan_scc(&graph);
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0], vec!["TreeNode".to_string()]);

        // Back-edges should detect the self-edge
        let members = vec!["TreeNode".to_string()];
        let back = spanning_tree_back_edges("TreeNode", &members, &graph);
        assert!(back.contains(&("TreeNode".to_string(), "TreeNode".to_string())));
    }

    // ---- SCC: two-type mutual recursion (Author/Book) ----

    #[test]
    fn test_scc_mutual_recursion() {
        let mut graph: HashMap<String, HashSet<String>> = HashMap::new();
        graph.insert(
            "Author".to_string(),
            vec!["Book".to_string()].into_iter().collect(),
        );
        graph.insert(
            "Book".to_string(),
            vec!["Author".to_string()].into_iter().collect(),
        );

        let sccs = tarjan_scc(&graph);
        // Single SCC containing both
        assert_eq!(sccs.len(), 1);
        let scc = &sccs[0];
        assert_eq!(scc.len(), 2);
        assert!(scc.contains(&"Author".to_string()));
        assert!(scc.contains(&"Book".to_string()));

        // Sort members by NFC codepoint for deterministic processing
        let mut members = scc.clone();
        members.sort();
        let start = &members[0]; // "Author" < "Book"

        let back = spanning_tree_back_edges(start, &members, &graph);
        // DFS from Author: Author -> Book (tree edge), Book -> Author (back-edge)
        assert_eq!(back.len(), 1);
        assert!(back.contains(&("Book".to_string(), "Author".to_string())));

        let order = scc_processing_order(start, &members, &graph, &back);
        // Reverse post-order on spanning tree: Author is root, Book is leaf.
        // Post-order = [Book, Author], reversed = [Author, Book].
        // Author is the spanning tree root and comes first in reverse post-order.
        assert_eq!(order[0], "Author");
        assert_eq!(order[1], "Book");
    }

    // ---- NFC normalization tests ----

    #[test]
    fn test_nfc_normalization() {
        // cafe\u0301 (NFD) should normalize to caf\u00e9 (NFC)
        let nfd = "cafe\u{0301}";
        let nfc = normalize_identifier(nfd);
        assert_eq!(nfc, "caf\u{00e9}");
    }

    // ---- JSON round-trip ----

    #[test]
    fn test_contract_id_from_json() {
        let json = r#"{
            "name": "EmptyService",
            "version": 1,
            "methods": [],
            "serialization_modes": ["xlang"],
            "scoped": "shared",
            "requires": null
        }"#;
        let id = compute_contract_id_from_json(json).unwrap();
        assert_eq!(
            id,
            "d016f1c19d536b69c4fb2af96acce700da5c45bd6c4860b6c9ae408b4ca35438"
        );
    }

    #[test]
    fn test_canonical_bytes_from_json_service_contract() {
        let json = r#"{
            "name": "EmptyService",
            "version": 1,
            "methods": [],
            "serialization_modes": ["xlang"],
            "scoped": "shared",
            "requires": null
        }"#;
        let bytes = canonical_bytes_from_json("ServiceContract", json).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "32456d7074795365727669636502000c010c16786c616e6700fd02"
        );
    }

    #[test]
    fn test_canonical_bytes_from_json_typedef() {
        let json = r#"{
            "kind": "enum",
            "package": "test",
            "name": "Color",
            "fields": [],
            "enum_values": [
                {"name": "RED", "value": 0},
                {"name": "GREEN", "value": 1},
                {"name": "BLUE", "value": 2}
            ],
            "union_variants": []
        }"#;
        let bytes = canonical_bytes_from_json("TypeDef", json).unwrap();
        assert_eq!(
            hex::encode(&bytes),
            "01127465737416436f6c6f72000c030c0e5245440016475245454e0212424c554504000c"
        );
    }

    // ---- §11.3.2.3 rules: producer_language validation ----

    #[test]
    fn test_producer_language_required_when_native() {
        let sc = ServiceContract {
            name: "Foo".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["native".to_string()],
            scoped: ScopeKind::Shared,
            requires: None,
            producer_language: String::new(),
        };
        let err = canonical_xlang_bytes_service_contract_checked(&sc).unwrap_err();
        assert!(
            err.to_string().contains("producer_language is empty"),
            "expected missing-producer-language error, got: {}",
            err
        );
    }

    #[test]
    fn test_producer_language_forbidden_when_xlang_only() {
        let sc = ServiceContract {
            name: "Foo".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["xlang".to_string()],
            scoped: ScopeKind::Shared,
            requires: None,
            producer_language: "python".to_string(),
        };
        let err = canonical_xlang_bytes_service_contract_checked(&sc).unwrap_err();
        assert!(
            err.to_string().contains("must be empty unless native"),
            "expected forbidden-producer-language error, got: {}",
            err
        );
    }

    #[test]
    fn test_producer_language_must_be_known() {
        let sc = ServiceContract {
            name: "Foo".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["native".to_string()],
            scoped: ScopeKind::Shared,
            requires: None,
            producer_language: "cobol".to_string(),
        };
        let err = canonical_xlang_bytes_service_contract_checked(&sc).unwrap_err();
        assert!(
            err.to_string()
                .contains("not a recognized language identifier"),
            "expected unknown-language error, got: {}",
            err
        );
    }

    #[test]
    fn test_producer_language_native_python_canonicalizes() {
        let sc = ServiceContract {
            name: "Foo".to_string(),
            version: 1,
            methods: vec![],
            serialization_modes: vec!["native".to_string()],
            scoped: ScopeKind::Shared,
            requires: None,
            producer_language: "python".to_string(),
        };
        let bytes = canonical_xlang_bytes_service_contract_checked(&sc).unwrap();
        // Same schema with producer_language = "typescript" should produce
        // different bytes (and therefore different contract_id). Core
        // property of the producer-owned rule for native mode.
        let sc_ts = ServiceContract {
            producer_language: "typescript".to_string(),
            ..sc.clone()
        };
        let bytes_ts = canonical_xlang_bytes_service_contract_checked(&sc_ts).unwrap();
        assert_ne!(bytes, bytes_ts);
    }

    // ---- §11.3.2.3 rules: field ID from NFC-name-sort ----

    #[test]
    fn test_field_id_from_nfc_sort_position() {
        // Author declares fields in order: `name`, `books`. Under the
        // NFC-name-sort rule, `books` comes before `name` (b < n), so the
        // canonical field IDs are books=1, name=2, regardless of what the
        // caller supplied.
        let td = TypeDef {
            kind: TypeDefKind::Message,
            package: "example".to_string(),
            name: "Author".to_string(),
            fields: vec![
                // Caller supplies ID=42, name-first order. Both are ignored;
                // canonicalization derives its own IDs and order.
                simple_primitive_field(42, "name", "string"),
                simple_primitive_field(99, "books", "string"),
            ],
            enum_values: vec![],
            union_variants: vec![],
        };
        let bytes = canonical_xlang_bytes_type_def(&td);

        // Same schema with declaration order reversed — should produce
        // byte-identical canonical bytes because IDs derive from names.
        let td2 = TypeDef {
            kind: TypeDefKind::Message,
            package: "example".to_string(),
            name: "Author".to_string(),
            fields: vec![
                simple_primitive_field(1, "books", "string"),
                simple_primitive_field(2, "name", "string"),
            ],
            enum_values: vec![],
            union_variants: vec![],
        };
        let bytes2 = canonical_xlang_bytes_type_def(&td2);

        assert_eq!(
            bytes, bytes2,
            "NFC-name-sorted canonicalization must be declaration-order \
             independent (Java determinism fix)"
        );
    }

    // ---- §11.3.2.3 rules: required=false with default_value ----

    #[test]
    fn test_default_value_in_canonical_bytes() {
        // Two TypeDefs that differ only in one field's default value:
        // should produce different canonical bytes, therefore different
        // contract_ids. Verifies defaults participate in identity.
        let td_default_empty = TypeDef {
            kind: TypeDefKind::Message,
            package: "test".to_string(),
            name: "Msg".to_string(),
            fields: vec![FieldDef {
                id: 1,
                name: "status".to_string(),
                type_kind: TypeKind::Primitive,
                type_primitive: "string".to_string(),
                type_ref: String::new(),
                self_ref_name: String::new(),
                optional: false,
                ref_tracked: false,
                container: ContainerKind::None,
                container_key_kind: TypeKind::Primitive,
                container_key_primitive: String::new(),
                container_key_ref: String::new(),
                required: false,
                default_value: String::new(), // empty string default
            }],
            enum_values: vec![],
            union_variants: vec![],
        };

        let td_default_idle = TypeDef {
            fields: vec![FieldDef {
                default_value: "086964".to_string(), // arbitrary non-empty hex
                ..td_default_empty.fields[0].clone()
            }],
            ..td_default_empty.clone()
        };

        let bytes_empty = canonical_xlang_bytes_type_def(&td_default_empty);
        let bytes_idle = canonical_xlang_bytes_type_def(&td_default_idle);
        assert_ne!(
            bytes_empty, bytes_idle,
            "changing a default value must change canonical bytes (defaults \
             are part of contract_id per §11.3.2.3)"
        );
    }

    #[test]
    fn test_required_vs_zero_default_distinct() {
        // required=true vs required=false with empty default must produce
        // different canonical bytes.
        let td_required = TypeDef {
            kind: TypeDefKind::Message,
            package: "test".to_string(),
            name: "Msg".to_string(),
            fields: vec![simple_primitive_field(1, "name", "string")], // required=true
            enum_values: vec![],
            union_variants: vec![],
        };

        let td_not_required = TypeDef {
            fields: vec![FieldDef {
                required: false,
                default_value: String::new(),
                ..td_required.fields[0].clone()
            }],
            ..td_required.clone()
        };

        let bytes_req = canonical_xlang_bytes_type_def(&td_required);
        let bytes_not_req = canonical_xlang_bytes_type_def(&td_not_required);
        assert_ne!(
            bytes_req, bytes_not_req,
            "'required' (no default) must differ from 'default = zero-value' \
             per §11.3.2.3"
        );
    }
}
