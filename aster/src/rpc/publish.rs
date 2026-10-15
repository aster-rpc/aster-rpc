//! Manifest publication and fetch/verify (Aster-SPEC §11.4).
//!
//! Requires the `rpc` + `docs` + `blobs` features. [`publish_contract`] uploads
//! a service's contract collection (`contract.bin` + `types/*.bin` +
//! `manifest.json`) to the blob store and writes the ArtifactRef, a manifest
//! shortcut, and a version pointer into a registry doc.
//! [`fetch_and_verify_contract`] resolves a service back to its collection,
//! downloads it, and checks `blake3(contract.bin) == contract_id`.

use aster_transport_core::contract::{
    build_contract_collection, compute_contract_id, compute_type_hash, decode_type_def_bytes,
    ContractManifest, ServiceContract, TypeDef,
};
use aster_transport_core::dynamic::DynContract;
use aster_transport_core::registry::{contract_key, manifest_key, version_key, ArtifactRef};

use crate::blobs::Blobs;
use crate::docs::Doc;
use crate::error::{Error, Result};
use crate::id::{AuthorId, Hash, NodeId};

/// Outcome of [`publish_contract`].
#[derive(Debug, Clone)]
pub struct PublishedContract {
    /// The published contract id (hex BLAKE3 of `contract.bin`).
    pub contract_id: String,
    /// The uploaded collection's hash.
    pub collection_hash: Hash,
}

/// A contract fetched from the registry and **verified** against its id —
/// returning this value means `blake3(contract.bin) == contract_id`, the
/// manifest's self-reported id matched, and every `types/{hash}.bin` hashed to
/// its name. Verification failures are errors.
#[derive(Debug, Clone)]
pub struct FetchedContract {
    pub contract_id: String,
    pub manifest: ContractManifest,
    /// The contract's payload `TypeDef`s (`types/{hash}.bin`).
    pub type_defs: Vec<TypeDef>,
}

impl FetchedContract {
    /// The contract bound to the dynamic Fory codec, ready for a
    /// [`DynamicClient`](super::DynamicClient).
    pub fn dyn_contract(&self) -> Result<DynContract> {
        DynContract::from_manifest(&self.manifest, self.type_defs.clone())
            .map_err(|e| Error::InvalidArgument(format!("contract types: {e}")))
    }
}

/// Publish `sc` (+ its `type_defs`) into the `registry` doc: build + upload the
/// contract collection, then write the ArtifactRef (`contracts/{id}`), the
/// manifest shortcut (`manifests/{id}`), and the version pointer
/// (`services/{name}/versions/v{n}`), all authored by `author`.
pub async fn publish_contract(
    registry: &Doc,
    blobs: &Blobs,
    author: &AuthorId,
    sc: &ServiceContract,
    type_defs: &[TypeDef],
    published_at_epoch_ms: i64,
) -> Result<PublishedContract> {
    let staged = stage_contract(blobs, author, sc, type_defs, published_at_epoch_ms).await?;
    advertise_contract(registry, author, &staged, &mut Vec::new()).await?;
    Ok(staged.published)
}

/// A contract collection uploaded to the blob store but not yet referenced
/// from any registry doc: content-addressed and unannounced, so staging it
/// discloses nothing.
pub(crate) struct StagedContract {
    pub(crate) published: PublishedContract,
    name: String,
    version: i32,
    artifact_json: Vec<u8>,
    manifest_json: Vec<u8>,
}

impl StagedContract {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn version(&self) -> i32 {
        self.version
    }
}

/// Phase one of publication: build and upload the collection.
pub(crate) async fn stage_contract(
    blobs: &Blobs,
    author: &AuthorId,
    sc: &ServiceContract,
    type_defs: &[TypeDef],
    published_at_epoch_ms: i64,
) -> Result<StagedContract> {
    let entries = build_contract_collection(sc, type_defs, author.as_str(), published_at_epoch_ms)?;
    let contract_bytes = collection_entry(&entries, "contract.bin")?;
    let manifest_json = collection_entry(&entries, "manifest.json")?;
    let contract_id = compute_contract_id(&contract_bytes);

    let collection_hash = blobs.add_collection(entries).await?;

    let artifact = ArtifactRef {
        contract_id: contract_id.clone(),
        collection_hash: collection_hash.as_str().to_string(),
        provider_endpoint_id: None,
        relay_url: None,
        ticket: None,
        published_by: author.as_str().to_string(),
        published_at_epoch_ms,
        collection_format: "index".to_string(),
    };
    let artifact_json =
        serde_json::to_vec(&artifact).map_err(|e| json_err("artifact encode", e))?;
    Ok(StagedContract {
        published: PublishedContract {
            contract_id,
            collection_hash,
        },
        name: sc.name.clone(),
        version: sc.version,
        artifact_json,
        manifest_json,
    })
}

/// Phase two: reference a staged collection from `registry`. The version
/// pointer -- the entry a consumer finds the service by -- is written last.
/// Each key is appended to `written` before it is written, so a caller can
/// retract a partial advertisement with [`retract_keys`].
pub(crate) async fn advertise_contract(
    registry: &Doc,
    author: &AuthorId,
    staged: &StagedContract,
    written: &mut Vec<Vec<u8>>,
) -> Result<()> {
    let id = &staged.published.contract_id;
    for (key, value) in [
        (contract_key(id), staged.artifact_json.clone()),
        (manifest_key(id), staged.manifest_json.clone()),
        (
            version_key(&staged.name, staged.version),
            id.clone().into_bytes(),
        ),
    ] {
        written.push(key.clone());
        registry.set_bytes(author, key, value).await?;
    }
    Ok(())
}

/// Delete advertised `keys` from `registry`, version pointers first so a
/// service stops being discoverable before its artifacts go. Every key is
/// attempted; the first error is returned.
pub(crate) async fn retract_keys(
    registry: &Doc,
    author: &AuthorId,
    keys: &[Vec<u8>],
) -> Result<()> {
    let mut ordered: Vec<&Vec<u8>> = keys
        .iter()
        .filter(|k| k.starts_with(b"services/"))
        .collect();
    ordered.extend(keys.iter().filter(|k| !k.starts_with(b"services/")));
    let mut first = None;
    for key in ordered {
        if let Err(err) = registry.del(author, key.clone()).await {
            first.get_or_insert(err);
        }
    }
    first.map_or(Ok(()), Err)
}

/// Resolve `(service, version)` in the `registry` doc → fetch the contract
/// collection from `producer` → verify `blake3(contract.bin) == contract_id`
/// and parse the manifest. The registry doc must already be readable locally
/// (created here, or synced from the producer).
pub async fn fetch_and_verify_contract(
    registry: &Doc,
    blobs: &Blobs,
    producer: &NodeId,
    service: &str,
    version: i32,
) -> Result<FetchedContract> {
    let cid_bytes = read_value(registry, version_key(service, version))
        .await?
        .ok_or_else(|| {
            Error::DocNotFound(format!("no version pointer for {service} v{version}"))
        })?;
    let contract_id = String::from_utf8(cid_bytes)
        .map_err(|_| Error::InvalidArgument("version pointer is not UTF-8".into()))?;

    let artifact_json = read_value(registry, contract_key(&contract_id))
        .await?
        .ok_or_else(|| Error::DocNotFound(format!("no artifact for contract {contract_id}")))?;
    let artifact: ArtifactRef =
        serde_json::from_slice(&artifact_json).map_err(|e| json_err("artifact decode", e))?;

    let collection_hash = Hash::from_hex(artifact.collection_hash.clone());
    let entries = blobs
        .download_collection(&collection_hash, producer)
        .await?;
    let contract_bytes = collection_entry(&entries, "contract.bin")?;

    // Fail closed: the fetched contract.bin MUST hash to the resolved id.
    let actual = compute_contract_id(&contract_bytes);
    if actual != contract_id {
        return Err(Error::Transport(anyhow::anyhow!(
            "contract integrity check failed: blake3(contract.bin) = {actual}, expected {contract_id}"
        )));
    }

    let manifest_json = collection_entry(&entries, "manifest.json")?;
    let manifest = ContractManifest::from_json(
        std::str::from_utf8(&manifest_json)
            .map_err(|_| Error::InvalidArgument("manifest.json is not UTF-8".into()))?,
    )
    .map_err(|e| Error::Transport(anyhow::anyhow!("manifest decode: {e}")))?;

    // The manifest's self-reported id must also match the resolved id.
    if manifest.contract_id != contract_id {
        return Err(Error::Transport(anyhow::anyhow!(
            "manifest contract_id = {}, expected {contract_id}",
            manifest.contract_id
        )));
    }

    // Payload types: each blob must hash to its name.
    let mut type_defs = Vec::new();
    for (name, bytes) in &entries {
        let Some(hash) = name
            .strip_prefix("types/")
            .and_then(|n| n.strip_suffix(".bin"))
        else {
            continue;
        };
        let actual = hex::encode(compute_type_hash(bytes));
        if !actual.eq_ignore_ascii_case(hash) {
            return Err(Error::Transport(anyhow::anyhow!(
                "type integrity check failed: blake3({name}) = {actual}"
            )));
        }
        type_defs.push(
            decode_type_def_bytes(bytes)
                .map_err(|e| Error::Transport(anyhow::anyhow!("{name}: {e}")))?,
        );
    }

    Ok(FetchedContract {
        contract_id,
        manifest,
        type_defs,
    })
}

fn collection_entry(entries: &[(String, Vec<u8>)], name: &str) -> Result<Vec<u8>> {
    entries
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, b)| b.clone())
        .ok_or_else(|| Error::InvalidArgument(format!("collection missing {name}")))
}

async fn read_value(doc: &Doc, key: Vec<u8>) -> Result<Option<Vec<u8>>> {
    match doc.query_latest_exact(key).await? {
        Some(entry) => Ok(Some(doc.read_entry_content(&entry.content_hash).await?)),
        None => Ok(None),
    }
}

fn json_err(ctx: &str, e: serde_json::Error) -> Error {
    Error::Transport(anyhow::anyhow!("{ctx}: {e}"))
}
