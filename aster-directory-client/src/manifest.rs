//! A contract manifest in the form the @aster service hashes: Python's
//! `ContractManifest.from_json(text).to_json(indent=None)`, which is
//! `json.dumps(asdict(manifest))`. Top-level keys follow the dataclass field
//! order with defaults filled in; method entries keep their key order from
//! the file. The directory contract ID is the BLAKE3 hex digest of that text.

use anyhow::{Context, Result, bail, ensure};

use crate::pyjson::{DumpOptions, PyJson};

/// `bindings/python/aster/contract/manifest.py`, in declaration order.
type FieldDefault = fn() -> PyJson;

const FIELDS: &[(&str, FieldDefault)] = &[
    ("v", || PyJson::Int(1)),
    ("service", || PyJson::Str(String::new())),
    ("version", || PyJson::Int(1)),
    ("contract_id", || PyJson::Str(String::new())),
    ("canonical_encoding", || {
        PyJson::Str("fory-xlang/0.15".into())
    }),
    ("type_count", || PyJson::Int(0)),
    ("type_hashes", || PyJson::Array(Vec::new())),
    ("method_count", || PyJson::Int(0)),
    ("methods", || PyJson::Array(Vec::new())),
    ("serialization_modes", || PyJson::Array(Vec::new())),
    ("producer_language", || PyJson::Str(String::new())),
    ("scoped", || PyJson::Str("shared".into())),
    ("description", || PyJson::Str(String::new())),
    ("tags", || PyJson::Array(Vec::new())),
    ("deprecated", || PyJson::Bool(false)),
    ("semver", || PyJson::Null),
    ("vcs_revision", || PyJson::Null),
    ("vcs_tag", || PyJson::Null),
    ("vcs_url", || PyJson::Null),
    ("changelog", || PyJson::Null),
    ("published_by", || PyJson::Str(String::new())),
    ("published_at_epoch_ms", || PyJson::Int(0)),
];

/// Python silently truncates beyond these (`aster/limits.py`); refusing
/// keeps the contract ID honest.
const MAX_METHODS: usize = 10_000;
const MAX_TYPE_HASHES: usize = 100_000;
const MAX_FIELDS_PER_METHOD: usize = 1_000;

#[derive(Debug, Clone)]
pub struct CanonicalManifest {
    pub service: String,
    pub version: i128,
    /// What `manifest_json` carries in a publish request.
    pub json: String,
    /// BLAKE3 of `json`: the directory's contract ID.
    pub contract_id: String,
}

/// Canonicalise one manifest object (as text).
pub fn canonical(text: &str) -> Result<CanonicalManifest> {
    let PyJson::Object(fields) = PyJson::parse(text).context("manifest is not JSON")? else {
        bail!("a manifest is a JSON object");
    };
    for (key, _) in &fields {
        ensure!(
            FIELDS.iter().any(|(k, _)| k == key),
            "unknown manifest field `{key}`"
        );
    }
    let get = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    ensure!(
        !matches!(get("v"), None | Some(PyJson::Null)),
        "manifest has no field-schema version `v`; regenerate it with a current Aster"
    );
    let mut out = Vec::with_capacity(FIELDS.len());
    for (name, default) in FIELDS {
        let mut value = get(name).unwrap_or_else(default);
        if matches!(*name, "version" | "method_count" | "type_count") {
            value = PyJson::Int(as_int(&value).with_context(|| format!("`{name}`"))?);
        }
        out.push((name.to_string(), value));
    }
    let manifest = PyJson::Object(out);
    if let Some(PyJson::Array(methods)) = manifest.get("methods") {
        ensure!(methods.len() <= MAX_METHODS, "too many methods");
        for m in methods {
            if let Some(PyJson::Array(f)) = m.get("fields") {
                ensure!(
                    f.len() <= MAX_FIELDS_PER_METHOD,
                    "too many fields in a method"
                );
            }
        }
    }
    if let Some(PyJson::Array(h)) = manifest.get("type_hashes") {
        ensure!(h.len() <= MAX_TYPE_HASHES, "too many type hashes");
    }
    let json = manifest.dumps(DumpOptions::PYTHON_DEFAULT)?;
    let service = match manifest.get("service") {
        Some(PyJson::Str(s)) => s.clone(),
        _ => bail!("`service` must be a string"),
    };
    let version = as_int(manifest.get("version").expect("filled"))?;
    Ok(CanonicalManifest {
        contract_id: blake3::hash(json.as_bytes()).to_hex().to_string(),
        service,
        version,
        json,
    })
}

/// A manifest file holds one manifest object or a list of them.
pub fn canonical_all(text: &str) -> Result<Vec<CanonicalManifest>> {
    match PyJson::parse(text).context("manifest file is not JSON")? {
        PyJson::Array(items) => items
            .iter()
            .map(|m| canonical(&m.dumps(DumpOptions::PYTHON_DEFAULT)?))
            .collect(),
        _ => Ok(vec![canonical(text)?]),
    }
}

/// Python's `int(x)` for the values a manifest plausibly holds.
fn as_int(v: &PyJson) -> Result<i128> {
    Ok(match v {
        PyJson::Int(i) => *i,
        PyJson::Bool(b) => i128::from(*b),
        PyJson::Float(f) if f.is_finite() => f.trunc() as i128,
        PyJson::Str(s) => s.trim().parse()?,
        _ => bail!("expected an integer"),
    })
}
