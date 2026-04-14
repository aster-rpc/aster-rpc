//! Aster service registry logic.
//!
//! Spec references:
//! - §11.2: Key schema
//! - §11.2.1: ArtifactRef
//! - §11.6: EndpointLease + health state machine
//! - §11.7: GossipEvent types
//! - §11.8: Publication and resolution flows
//! - §11.9: Mandatory filters + ranking strategies
//!
//! This module centralizes resolution, publishing, and ACL logic so that every
//! language binding gets identical behavior. The Python reference implementation
//! lives at `bindings/python/aster/registry/`; Rust is the normative source of
//! truth going forward.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::{CoreDoc, CoreDocEntry, CoreGossipTopic};

// ════════════════════════════════════════════════════════════════════════════
// Health status (§11.6)
// ════════════════════════════════════════════════════════════════════════════

pub const HEALTH_STARTING: &str = "starting";
pub const HEALTH_READY: &str = "ready";
pub const HEALTH_DEGRADED: &str = "degraded";
pub const HEALTH_DRAINING: &str = "draining";

/// Return true if the given health string is READY or DEGRADED.
pub fn is_routable(status: &str) -> bool {
    status == HEALTH_READY || status == HEALTH_DEGRADED
}

// ════════════════════════════════════════════════════════════════════════════
// Gossip event type (§11.7)
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "i32", into = "i32")]
pub enum GossipEventType {
    ContractPublished,
    ChannelUpdated,
    EndpointLeaseUpserted,
    EndpointDown,
    AclChanged,
    CompatibilityPublished,
}

impl From<i32> for GossipEventType {
    fn from(v: i32) -> Self {
        match v {
            0 => Self::ContractPublished,
            1 => Self::ChannelUpdated,
            2 => Self::EndpointLeaseUpserted,
            3 => Self::EndpointDown,
            4 => Self::AclChanged,
            _ => Self::CompatibilityPublished,
        }
    }
}

impl From<GossipEventType> for i32 {
    fn from(v: GossipEventType) -> i32 {
        match v {
            GossipEventType::ContractPublished => 0,
            GossipEventType::ChannelUpdated => 1,
            GossipEventType::EndpointLeaseUpserted => 2,
            GossipEventType::EndpointDown => 3,
            GossipEventType::AclChanged => 4,
            GossipEventType::CompatibilityPublished => 5,
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Wire types (mirror bindings/python/aster/registry/models.py)
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSummary {
    pub name: String,
    pub version: i32,
    pub contract_id: String,
    #[serde(default)]
    pub channels: BTreeMap<String, String>,
    #[serde(default = "default_pattern")]
    pub pattern: String,
    #[serde(default)]
    pub serialization_modes: Vec<String>,
}

fn default_pattern() -> String {
    "shared".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub contract_id: String,
    pub collection_hash: String,
    #[serde(default)]
    pub provider_endpoint_id: Option<String>,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub ticket: Option<String>,
    #[serde(default)]
    pub published_by: String,
    #[serde(default)]
    pub published_at_epoch_ms: i64,
    #[serde(default = "default_collection_format")]
    pub collection_format: String,
}

fn default_collection_format() -> String {
    "raw".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointLease {
    pub endpoint_id: String,
    pub contract_id: String,
    pub service: String,
    pub version: i32,
    pub lease_expires_epoch_ms: i64,
    pub lease_seq: i64,
    pub alpn: String,
    #[serde(default)]
    pub serialization_modes: Vec<String>,
    #[serde(default)]
    pub feature_flags: Vec<String>,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub direct_addrs: Vec<String>,
    #[serde(default)]
    pub load: Option<f32>,
    #[serde(default)]
    pub language_runtime: Option<String>,
    #[serde(default)]
    pub aster_version: String,
    #[serde(default)]
    pub policy_realm: Option<String>,
    pub health_status: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub updated_at_epoch_ms: i64,
}

impl EndpointLease {
    /// Return true if (now - updated_at) is within the lease duration window.
    pub fn is_fresh(&self, lease_duration_s: i32) -> bool {
        let now_ms = now_epoch_ms();
        (now_ms - self.updated_at_epoch_ms) <= (lease_duration_s as i64) * 1000
    }

    pub fn is_routable(&self) -> bool {
        is_routable(&self.health_status)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipEvent {
    pub r#type: GossipEventType,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub version: Option<i32>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub contract_id: Option<String>,
    #[serde(default)]
    pub endpoint_id: Option<String>,
    #[serde(default)]
    pub key_prefix: Option<String>,
    #[serde(default)]
    pub timestamp_ms: i64,
}

// ════════════════════════════════════════════════════════════════════════════
// Key helpers (§11.2, §12.4)
// ════════════════════════════════════════════════════════════════════════════

pub fn contract_key(contract_id: &str) -> Vec<u8> {
    format!("contracts/{}", contract_id).into_bytes()
}

pub fn version_key(name: &str, version: i32) -> Vec<u8> {
    format!("services/{}/versions/v{}", name, version).into_bytes()
}

pub fn channel_key(name: &str, channel: &str) -> Vec<u8> {
    format!("services/{}/channels/{}", name, channel).into_bytes()
}

pub fn tag_key(name: &str, tag: &str) -> Vec<u8> {
    format!("services/{}/tags/{}", name, tag).into_bytes()
}

pub fn lease_key(name: &str, contract_id: &str, endpoint_id: &str) -> Vec<u8> {
    format!(
        "services/{}/contracts/{}/endpoints/{}",
        name, contract_id, endpoint_id
    )
    .into_bytes()
}

pub fn lease_prefix(name: &str, contract_id: &str) -> Vec<u8> {
    format!("services/{}/contracts/{}/endpoints/", name, contract_id).into_bytes()
}

pub fn acl_key(subkey: &str) -> Vec<u8> {
    format!("_aster/acl/{}", subkey).into_bytes()
}

pub fn config_key(subkey: &str) -> Vec<u8> {
    format!("_aster/config/{}", subkey).into_bytes()
}

pub const REGISTRY_PREFIXES: &[&[u8]] = &[
    b"contracts/",
    b"services/",
    b"endpoints/",
    b"compatibility/",
    b"_aster/",
];

// ════════════════════════════════════════════════════════════════════════════
// Filter + ranking logic (§11.9)
// ════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub service: String,
    pub version: Option<i32>,
    pub channel: Option<String>,
    pub contract_id: Option<String>,
    pub strategy: String,
    pub caller_alpn: String,
    pub caller_serialization_modes: Vec<String>,
    pub caller_policy_realm: Option<String>,
    pub lease_duration_s: i32,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            service: String::new(),
            version: None,
            channel: None,
            contract_id: None,
            strategy: "round_robin".to_string(),
            caller_alpn: "aster/1".to_string(),
            caller_serialization_modes: vec!["fory-xlang".to_string()],
            caller_policy_realm: None,
            lease_duration_s: 45,
        }
    }
}

/// Apply the 5 mandatory filters (§11.9) in normative order:
/// 1. health in {READY, DEGRADED}
/// 2. lease freshness
/// 3. ALPN match
/// 4. serialization_modes overlap
/// 5. policy_realm compatibility
pub fn apply_mandatory_filters(
    leases: Vec<EndpointLease>,
    opts: &ResolveOptions,
) -> Vec<EndpointLease> {
    let caller_modes: std::collections::HashSet<&str> = opts
        .caller_serialization_modes
        .iter()
        .map(|s| s.as_str())
        .collect();

    leases
        .into_iter()
        .filter(|lease| {
            if !lease.is_routable() {
                return false;
            }
            if !lease.is_fresh(opts.lease_duration_s) {
                return false;
            }
            if !opts.caller_alpn.is_empty() && lease.alpn != opts.caller_alpn {
                return false;
            }
            if !caller_modes.is_empty() {
                let overlap = lease
                    .serialization_modes
                    .iter()
                    .any(|m| caller_modes.contains(m.as_str()));
                if !overlap {
                    return false;
                }
            }
            if let Some(ref caller_realm) = opts.caller_policy_realm {
                if let Some(ref lease_realm) = lease.policy_realm {
                    if lease_realm != caller_realm {
                        return false;
                    }
                }
            }
            true
        })
        .collect()
}

/// Round-robin state keyed by contract_id.
#[derive(Default)]
pub struct ResolveState {
    rr: Mutex<HashMap<String, usize>>,
    /// Latest lease_seq per (service, contract_id, endpoint_id) — stale rejection.
    seq_cache: Mutex<HashMap<(String, String, String), i64>>,
}

impl ResolveState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reject leases whose lease_seq is <= the latest observed for that key.
    pub fn filter_monotonic(&self, leases: Vec<EndpointLease>) -> Vec<EndpointLease> {
        let mut cache = self.seq_cache.lock().unwrap();
        leases
            .into_iter()
            .filter(|lease| {
                let key = (
                    lease.service.clone(),
                    lease.contract_id.clone(),
                    lease.endpoint_id.clone(),
                );
                let latest = cache.get(&key).copied().unwrap_or(0);
                if lease.lease_seq <= latest {
                    false
                } else {
                    cache.insert(key, lease.lease_seq);
                    true
                }
            })
            .collect()
    }

    /// Rank survivors by strategy. READY entries always precede DEGRADED.
    pub fn rank(
        &self,
        candidates: Vec<EndpointLease>,
        strategy: &str,
        contract_id: &str,
    ) -> Vec<EndpointLease> {
        if candidates.is_empty() {
            return candidates;
        }

        let mut ready: Vec<EndpointLease> = candidates
            .iter()
            .filter(|l| l.health_status == HEALTH_READY)
            .cloned()
            .collect();
        let mut degraded: Vec<EndpointLease> = candidates
            .iter()
            .filter(|l| l.health_status == HEALTH_DEGRADED)
            .cloned()
            .collect();

        self.apply_strategy(&mut ready, strategy, contract_id);
        self.apply_strategy(&mut degraded, strategy, contract_id);

        let mut result = ready;
        result.extend(degraded);
        result
    }

    fn apply_strategy(&self, group: &mut [EndpointLease], strategy: &str, contract_id: &str) {
        if group.is_empty() {
            return;
        }
        match strategy {
            "round_robin" => {
                let mut rr = self.rr.lock().unwrap();
                let state = rr.entry(contract_id.to_string()).or_insert(0);
                let idx = *state % group.len();
                *state = (idx + 1) % group.len();
                group.rotate_left(idx);
            }
            "least_load" => {
                // Stable partition: leases with load first (sorted asc), then leases without.
                group.sort_by(|a, b| match (a.load, b.load) {
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
            }
            "random" => {
                // Cheap shuffle via timestamp-based LCG — good enough for load spreading.
                let mut seed = now_epoch_ms() as u64;
                for i in (1..group.len()).rev() {
                    seed = seed
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    let j = (seed as usize) % (i + 1);
                    group.swap(i, j);
                }
            }
            _ => {}
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Doc I/O — resolution pipeline (§11.8)
// ════════════════════════════════════════════════════════════════════════════

/// Read a single contract pointer (version_key or channel_key) and return the
/// contract_id it points to. Picks the entry with the highest timestamp.
async fn read_pointer(
    doc: &CoreDoc,
    key: Vec<u8>,
    acl: Option<&RegistryAcl>,
) -> Result<Option<String>> {
    let mut entries = doc.query_key_exact(key).await?;
    if let Some(acl) = acl {
        entries = acl.filter_trusted(entries);
    }
    if entries.is_empty() {
        return Ok(None);
    }
    entries.sort_by_key(|e| e.timestamp);
    let entry = entries.last().unwrap();
    let bytes = doc.read_entry_content(entry.content_hash.clone()).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(bytes)?.trim().to_string()))
}

/// Resolve a service/version/channel/contract_id to a contract_id string.
pub async fn resolve_contract_id(
    doc: &CoreDoc,
    service: &str,
    version: Option<i32>,
    channel: Option<&str>,
    contract_id: Option<&str>,
    acl: Option<&RegistryAcl>,
) -> Result<Option<String>> {
    if let Some(cid) = contract_id {
        return Ok(Some(cid.to_string()));
    }
    if let Some(v) = version {
        if let Some(cid) = read_pointer(doc, version_key(service, v), acl).await? {
            return Ok(Some(cid));
        }
    }
    if let Some(c) = channel {
        if let Some(cid) = read_pointer(doc, channel_key(service, c), acl).await? {
            return Ok(Some(cid));
        }
    }
    Ok(None)
}

/// Read all EndpointLease entries under a (service, contract_id) prefix and parse them.
pub async fn list_leases(
    doc: &CoreDoc,
    service: &str,
    contract_id: &str,
    acl: Option<&RegistryAcl>,
) -> Result<Vec<EndpointLease>> {
    let prefix = lease_prefix(service, contract_id);
    let mut entries = doc.query_key_prefix(prefix).await?;
    if let Some(acl) = acl {
        entries = acl.filter_trusted(entries);
    }
    let mut leases = Vec::new();
    for entry in entries {
        let bytes = doc.read_entry_content(entry.content_hash).await?;
        if bytes.is_empty() || bytes == b"null" {
            continue;
        }
        match serde_json::from_slice::<EndpointLease>(&bytes) {
            Ok(lease) => leases.push(lease),
            Err(e) => tracing::debug!("skipping malformed lease entry: {}", e),
        }
    }
    Ok(leases)
}

/// Full resolve pipeline: pointer lookup → list → seq filter → mandatory filters → rank.
pub async fn resolve(
    doc: &CoreDoc,
    state: &ResolveState,
    opts: &ResolveOptions,
    acl: Option<&RegistryAcl>,
) -> Result<Option<EndpointLease>> {
    let cid = resolve_contract_id(
        doc,
        &opts.service,
        opts.version,
        opts.channel.as_deref(),
        opts.contract_id.as_deref(),
        acl,
    )
    .await?;
    let cid = match cid {
        Some(c) => c,
        None => return Ok(None),
    };
    let raw = list_leases(doc, &opts.service, &cid, acl).await?;
    let fresh = state.filter_monotonic(raw);
    let filtered = apply_mandatory_filters(fresh, opts);
    let ranked = state.rank(filtered, &opts.strategy, &cid);
    Ok(ranked.into_iter().next())
}

// ════════════════════════════════════════════════════════════════════════════
// Publishing (§11.8)
// ════════════════════════════════════════════════════════════════════════════

/// Write an EndpointLease to the doc and emit ENDPOINT_LEASE_UPSERTED gossip.
pub async fn publish_lease(
    doc: &CoreDoc,
    author_id: &str,
    lease: &EndpointLease,
    gossip: Option<&CoreGossipTopic>,
) -> Result<()> {
    let bytes = serde_json::to_vec(lease)?;
    let key = lease_key(&lease.service, &lease.contract_id, &lease.endpoint_id);
    doc.set_bytes(author_id.to_string(), key, bytes).await?;

    if let Some(topic) = gossip {
        let ev = GossipEvent {
            r#type: GossipEventType::EndpointLeaseUpserted,
            service: Some(lease.service.clone()),
            version: Some(lease.lease_seq as i32),
            channel: None,
            contract_id: Some(lease.contract_id.clone()),
            endpoint_id: Some(lease.endpoint_id.clone()),
            key_prefix: None,
            timestamp_ms: now_epoch_ms(),
        };
        let _ = topic.broadcast(serde_json::to_vec(&ev)?).await;
    }
    Ok(())
}

/// Write an ArtifactRef at contracts/{contract_id} and a version pointer.
/// Optionally emits CONTRACT_PUBLISHED gossip.
pub async fn publish_artifact(
    doc: &CoreDoc,
    author_id: &str,
    artifact: &ArtifactRef,
    service: &str,
    version: i32,
    channel: Option<&str>,
    gossip: Option<&CoreGossipTopic>,
) -> Result<()> {
    let ref_bytes = serde_json::to_vec(artifact)?;
    doc.set_bytes(
        author_id.to_string(),
        contract_key(&artifact.contract_id),
        ref_bytes,
    )
    .await?;
    doc.set_bytes(
        author_id.to_string(),
        version_key(service, version),
        artifact.contract_id.clone().into_bytes(),
    )
    .await?;
    if let Some(ch) = channel {
        doc.set_bytes(
            author_id.to_string(),
            channel_key(service, ch),
            artifact.contract_id.clone().into_bytes(),
        )
        .await?;
    }
    if let Some(topic) = gossip {
        let ev = GossipEvent {
            r#type: GossipEventType::ContractPublished,
            service: Some(service.to_string()),
            version: Some(version),
            channel: channel.map(String::from),
            contract_id: Some(artifact.contract_id.clone()),
            endpoint_id: None,
            key_prefix: None,
            timestamp_ms: now_epoch_ms(),
        };
        let _ = topic.broadcast(serde_json::to_vec(&ev)?).await;
    }
    Ok(())
}

/// Renew an existing lease: update health, load, bump timestamps, rewrite row.
/// Reads the current lease from the doc, applies the update, and rewrites it.
#[allow(clippy::too_many_arguments)]
pub async fn renew_lease(
    doc: &CoreDoc,
    author_id: &str,
    service: &str,
    contract_id: &str,
    endpoint_id: &str,
    health: &str,
    load: Option<f32>,
    lease_duration_s: i32,
    gossip: Option<&CoreGossipTopic>,
) -> Result<()> {
    let key = lease_key(service, contract_id, endpoint_id);
    let entries = doc.query_key_exact(key.clone()).await?;
    let entry = entries
        .into_iter()
        .max_by_key(|e| e.timestamp)
        .ok_or_else(|| {
            anyhow!(
                "no lease found for {}/{}/{}",
                service,
                contract_id,
                endpoint_id
            )
        })?;
    let bytes = doc.read_entry_content(entry.content_hash).await?;
    let mut lease: EndpointLease = serde_json::from_slice(&bytes)?;

    lease.health_status = health.to_string();
    lease.load = load;
    lease.lease_seq += 1;
    let now_ms = now_epoch_ms();
    lease.updated_at_epoch_ms = now_ms;
    lease.lease_expires_epoch_ms = now_ms + (lease_duration_s as i64) * 1000;

    publish_lease(doc, author_id, &lease, gossip).await
}

// ════════════════════════════════════════════════════════════════════════════
// ACL (§11.2.3)
// ════════════════════════════════════════════════════════════════════════════

/// In-memory ACL cache for the registry doc.
///
/// Starts in *open mode* (all writers trusted) until `add_writer` is called
/// or `reload` loads a writers list. Open mode is appropriate for local/dev.
pub struct RegistryAcl {
    open: RwLock<bool>,
    writers: RwLock<std::collections::HashSet<String>>,
    readers: RwLock<std::collections::HashSet<String>>,
    admins: RwLock<std::collections::HashSet<String>>,
}

impl Default for RegistryAcl {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryAcl {
    pub fn new() -> Self {
        Self {
            open: RwLock::new(true),
            writers: RwLock::new(Default::default()),
            readers: RwLock::new(Default::default()),
            admins: RwLock::new(Default::default()),
        }
    }

    pub fn is_trusted_writer(&self, author_id: &str) -> bool {
        if *self.open.read().unwrap() {
            return true;
        }
        self.writers.read().unwrap().contains(author_id)
    }

    pub fn filter_trusted(&self, entries: Vec<CoreDocEntry>) -> Vec<CoreDocEntry> {
        entries
            .into_iter()
            .filter(|e| self.is_trusted_writer(&e.author_id))
            .collect()
    }

    pub fn writers(&self) -> Vec<String> {
        self.writers.read().unwrap().iter().cloned().collect()
    }

    /// Reload the writers/readers/admins lists from the doc. If no writers list
    /// exists, stay in open mode.
    pub async fn reload(&self, doc: &CoreDoc) -> Result<()> {
        let writers = read_acl_list(doc, "writers").await?;
        let Some(writers) = writers else {
            return Ok(());
        };
        let readers = read_acl_list(doc, "readers").await?.unwrap_or_default();
        let admins = read_acl_list(doc, "admins").await?.unwrap_or_default();
        *self.writers.write().unwrap() = writers.into_iter().collect();
        *self.readers.write().unwrap() = readers.into_iter().collect();
        *self.admins.write().unwrap() = admins.into_iter().collect();
        *self.open.write().unwrap() = false;
        Ok(())
    }

    pub async fn add_writer(&self, doc: &CoreDoc, author_id: &str, writer: &str) -> Result<()> {
        self.writers.write().unwrap().insert(writer.to_string());
        *self.open.write().unwrap() = false;
        self.persist(doc, author_id, "writers").await
    }

    pub async fn remove_writer(&self, doc: &CoreDoc, author_id: &str, writer: &str) -> Result<()> {
        self.writers.write().unwrap().remove(writer);
        self.persist(doc, author_id, "writers").await
    }

    async fn persist(&self, doc: &CoreDoc, author_id: &str, subkey: &str) -> Result<()> {
        let list: Vec<String> = match subkey {
            "writers" => self.writers.read().unwrap().iter().cloned().collect(),
            "readers" => self.readers.read().unwrap().iter().cloned().collect(),
            "admins" => self.admins.read().unwrap().iter().cloned().collect(),
            _ => return Err(anyhow!("unknown ACL subkey: {}", subkey)),
        };
        let bytes = serde_json::to_vec(&list)?;
        doc.set_bytes(author_id.to_string(), acl_key(subkey), bytes)
            .await?;
        Ok(())
    }
}

async fn read_acl_list(doc: &CoreDoc, subkey: &str) -> Result<Option<Vec<String>>> {
    let entries = doc.query_key_exact(acl_key(subkey)).await?;
    let Some(entry) = entries.into_iter().next() else {
        return Ok(None);
    };
    let bytes = doc.read_entry_content(entry.content_hash).await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let list: Vec<String> = serde_json::from_slice(&bytes)?;
    Ok(Some(list))
}

// ════════════════════════════════════════════════════════════════════════════
// Utilities
// ════════════════════════════════════════════════════════════════════════════

pub fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ════════════════════════════════════════════════════════════════════════════
// Tests — pure filter + ranking logic (no CoreDoc required)
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lease(
        endpoint_id: &str,
        health: &str,
        alpn: &str,
        load: Option<f32>,
        realm: Option<&str>,
        fresh: bool,
    ) -> EndpointLease {
        let now = now_epoch_ms();
        let updated = if fresh { now } else { now - 10 * 60 * 1000 };
        EndpointLease {
            endpoint_id: endpoint_id.to_string(),
            contract_id: "cid".to_string(),
            service: "svc".to_string(),
            version: 1,
            lease_expires_epoch_ms: now + 60_000,
            lease_seq: 1,
            alpn: alpn.to_string(),
            serialization_modes: vec!["fory-xlang".to_string()],
            feature_flags: vec![],
            relay_url: None,
            direct_addrs: vec![],
            load,
            language_runtime: None,
            aster_version: "0.1".to_string(),
            policy_realm: realm.map(String::from),
            health_status: health.to_string(),
            tags: vec![],
            updated_at_epoch_ms: updated,
        }
    }

    fn base_opts() -> ResolveOptions {
        ResolveOptions {
            service: "svc".to_string(),
            version: Some(1),
            strategy: "round_robin".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn filter_drops_starting() {
        let leases = vec![
            make_lease("a", HEALTH_STARTING, "aster/1", None, None, true),
            make_lease("b", HEALTH_READY, "aster/1", None, None, true),
        ];
        let out = apply_mandatory_filters(leases, &base_opts());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].endpoint_id, "b");
    }

    #[test]
    fn filter_drops_draining() {
        let leases = vec![make_lease(
            "a",
            HEALTH_DRAINING,
            "aster/1",
            None,
            None,
            true,
        )];
        assert!(apply_mandatory_filters(leases, &base_opts()).is_empty());
    }

    #[test]
    fn filter_drops_stale() {
        let leases = vec![make_lease("a", HEALTH_READY, "aster/1", None, None, false)];
        assert!(apply_mandatory_filters(leases, &base_opts()).is_empty());
    }

    #[test]
    fn filter_drops_wrong_alpn() {
        let leases = vec![make_lease("a", HEALTH_READY, "other/1", None, None, true)];
        assert!(apply_mandatory_filters(leases, &base_opts()).is_empty());
    }

    #[test]
    fn filter_drops_wrong_realm() {
        let mut opts = base_opts();
        opts.caller_policy_realm = Some("prod".to_string());
        let leases = vec![
            make_lease("a", HEALTH_READY, "aster/1", None, Some("dev"), true),
            make_lease("b", HEALTH_READY, "aster/1", None, Some("prod"), true),
            make_lease("c", HEALTH_READY, "aster/1", None, None, true),
        ];
        let out = apply_mandatory_filters(leases, &opts);
        let ids: Vec<_> = out.iter().map(|l| l.endpoint_id.clone()).collect();
        // Lease c (None realm) passes because lease realm is None.
        assert_eq!(ids, vec!["b", "c"]);
    }

    #[test]
    fn rank_ready_before_degraded() {
        let state = ResolveState::new();
        let leases = vec![
            make_lease("d1", HEALTH_DEGRADED, "aster/1", None, None, true),
            make_lease("r1", HEALTH_READY, "aster/1", None, None, true),
        ];
        let out = state.rank(leases, "round_robin", "cid");
        assert_eq!(out[0].endpoint_id, "r1");
        assert_eq!(out[1].endpoint_id, "d1");
    }

    #[test]
    fn rank_round_robin_rotates() {
        let state = ResolveState::new();
        let leases = vec![
            make_lease("a", HEALTH_READY, "aster/1", None, None, true),
            make_lease("b", HEALTH_READY, "aster/1", None, None, true),
            make_lease("c", HEALTH_READY, "aster/1", None, None, true),
        ];
        let out1 = state.rank(leases.clone(), "round_robin", "cid");
        let out2 = state.rank(leases.clone(), "round_robin", "cid");
        let out3 = state.rank(leases, "round_robin", "cid");
        assert_eq!(out1[0].endpoint_id, "a");
        assert_eq!(out2[0].endpoint_id, "b");
        assert_eq!(out3[0].endpoint_id, "c");
    }

    #[test]
    fn rank_least_load_prefers_lowest() {
        let state = ResolveState::new();
        let leases = vec![
            make_lease("hi", HEALTH_READY, "aster/1", Some(0.9), None, true),
            make_lease("lo", HEALTH_READY, "aster/1", Some(0.1), None, true),
            make_lease("mid", HEALTH_READY, "aster/1", Some(0.5), None, true),
        ];
        let out = state.rank(leases, "least_load", "cid");
        assert_eq!(out[0].endpoint_id, "lo");
        assert_eq!(out[1].endpoint_id, "mid");
        assert_eq!(out[2].endpoint_id, "hi");
    }

    #[test]
    fn filter_monotonic_rejects_stale_seq() {
        let state = ResolveState::new();
        let mut a = make_lease("a", HEALTH_READY, "aster/1", None, None, true);
        a.lease_seq = 5;
        let mut a_stale = a.clone();
        a_stale.lease_seq = 3;
        let first = state.filter_monotonic(vec![a.clone()]);
        assert_eq!(first.len(), 1);
        let second = state.filter_monotonic(vec![a_stale]);
        assert!(second.is_empty());
    }

    #[test]
    fn key_helpers_match_python() {
        assert_eq!(contract_key("abc"), b"contracts/abc");
        assert_eq!(version_key("svc", 7), b"services/svc/versions/v7");
        assert_eq!(
            channel_key("svc", "stable"),
            b"services/svc/channels/stable"
        );
        assert_eq!(
            lease_key("svc", "cid", "eid"),
            b"services/svc/contracts/cid/endpoints/eid"
        );
        assert_eq!(
            lease_prefix("svc", "cid"),
            b"services/svc/contracts/cid/endpoints/"
        );
        assert_eq!(acl_key("writers"), b"_aster/acl/writers");
    }

    #[test]
    fn acl_open_mode_trusts_all() {
        let acl = RegistryAcl::new();
        assert!(acl.is_trusted_writer("anything"));
    }

    #[test]
    fn gossip_event_round_trip() {
        let ev = GossipEvent {
            r#type: GossipEventType::EndpointDown,
            service: Some("svc".to_string()),
            version: None,
            channel: None,
            contract_id: None,
            endpoint_id: Some("eid".to_string()),
            key_prefix: None,
            timestamp_ms: 123,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: GossipEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.r#type, GossipEventType::EndpointDown);
        assert_eq!(parsed.endpoint_id.as_deref(), Some("eid"));
    }
}
