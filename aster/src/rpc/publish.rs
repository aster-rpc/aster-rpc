//! Manifest publication and fetch/verify (Aster-SPEC §11.4).
//!
//! Requires the `rpc` + `docs` + `blobs` features. [`publish_contract`] uploads
//! a service's contract collection (`contract.bin` + `types/*.bin` +
//! `manifest.json`) to the blob store and writes the ArtifactRef, a manifest
//! shortcut, and a version pointer into a registry doc.
//! [`fetch_and_verify_contract`] resolves a service back to its collection,
//! downloads it, and checks `blake3(contract.bin) == contract_id`.

use aster_transport_core::contract::{
    build_contract_collection, compute_contract_id, ContractManifest, ServiceContract, TypeDef,
};
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

/// A contract fetched from the registry and verified against its id.
#[derive(Debug, Clone)]
pub struct FetchedContract {
    pub contract_id: String,
    pub manifest: ContractManifest,
    /// `true` iff `blake3(contract.bin) == contract_id`.
    pub verified: bool,
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

    registry
        .set_bytes(author, contract_key(&contract_id), artifact_json)
        .await?;
    registry
        .set_bytes(author, manifest_key(&contract_id), manifest_json)
        .await?;
    registry
        .set_bytes(
            author,
            version_key(&sc.name, sc.version),
            contract_id.clone().into_bytes(),
        )
        .await?;

    Ok(PublishedContract {
        contract_id,
        collection_hash,
    })
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
    let manifest_json = collection_entry(&entries, "manifest.json")?;

    let verified = compute_contract_id(&contract_bytes) == contract_id;
    let manifest = ContractManifest::from_json(
        std::str::from_utf8(&manifest_json)
            .map_err(|_| Error::InvalidArgument("manifest.json is not UTF-8".into()))?,
    )
    .map_err(|e| Error::Transport(anyhow::anyhow!("manifest decode: {e}")))?;

    Ok(FetchedContract {
        contract_id,
        manifest,
        verified,
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
