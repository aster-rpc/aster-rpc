//! Issuer keys for provider tokens, replicated through the fleet namespace
//! (design: "Issuer keys: replicated, never fetched by the core" in
//! `docs/_internal/web_gateway/gateway-identity.md`).
//!
//! A core never fetches keys over HTTP. A publisher (a node, normally the
//! gateway, whose credential grants [`IDENTITY_PUBLISHER_ROLE`]) fetches an
//! identity provider's JWK set and writes it into the fleet namespace
//! Enrollment uses, with [`publish_issuer_keys`], at
//! `aster/identity/v1/issuers/<base64url(iss)>/jwks`
//! ([`issuer_keys_key`]). Every core reads it from there, so air-gapped cores
//! verify too.
//!
//! ## Who may publish
//!
//! A record's authority is its **docs author**. Aster nodes write as their
//! node identity (the default author *is* the node key), and Docs verifies
//! every entry's author signature, so an entry's author is the node that
//! wrote it. A core accepts a record only when its author's credential grants
//! `aster.identity-publisher`, through the server's Enrollment role mapping
//! (`[enrollment.roles]`), exactly as Gate 3 would map it. The publisher need
//! not have a session with the core: the record carries the publisher's
//! credential chain, verified as admission would (pinned root and audience,
//! delegation bounds, delegate and credential revocations, signed validity),
//! and an active root registration of the author takes precedence, as in
//! admission; a root denial makes the record count for nothing. A record
//! stops counting when the credential it rests on expires, and when a
//! revocation reaches the namespace.
//!
//! Whoever holds this role can forge any user of the issuers it publishes
//! for, so it is granted narrowly.
//!
//! ## What a core keeps
//!
//! The newest accepted record (by `fetched_at`) per issuer. A set older than
//! `[identity] max_key_set_age_secs` (default 48 hours) verifies nothing, so
//! a publisher that stops refreshing cannot keep a removed key alive. Keys of an issuer
//! that is not in `[[identity.trusted_issuers]]` are kept but never used, so
//! a NodeConfig override that adds an issuer takes effect as soon as it is
//! acknowledged. A token whose `kid` is not in its issuer's newest set is
//! refused with `UNAUTHENTICATED`; nothing is fetched. A key removed from the
//! newest set stops verifying when that set arrives. The cache is rebuilt on
//! every change to the namespace (and once a minute, so expiries and
//! late-applied revocations are picked up). When the root rotates, the
//! watcher re-opens on the new root's fleet namespace: nothing written under
//! the old root counts any more.

use std::collections::BTreeMap;
use std::sync::Arc;

use aster_transport_core::delegation::provider_token::ProviderKey;

pub use aster_transport_core::delegation::issuer_keys::{
    issuer_keys_key, KeySetError, ISSUER_KEYS_PREFIX, MAX_KEY_SET_RECORD_BYTES,
};

/// The role a publisher's credential must grant for its issuer key sets to
/// count. Granted through Enrollment, narrowly.
pub const IDENTITY_PUBLISHER_ROLE: &str = "aster.identity-publisher";
/// Most key-set entries a core reads; more and it holds none (fail closed).
pub const MAX_KEY_SET_ENTRIES: usize = 1024;

/// One accepted key set.
#[derive(Debug)]
struct Accepted {
    keys: Vec<ProviderKey>,
    publisher: [u8; 32],
    source_url: String,
    fetched_at: u64,
    /// When the publisher's credential (or registration) stops being valid.
    valid_until: Option<u64>,
}

/// What a server holds for one issuer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuerKeySet {
    pub iss: String,
    /// The publisher's node id.
    pub publisher: String,
    pub source_url: String,
    pub fetched_at: u64,
    /// The key ids, in the set's order.
    pub kids: Vec<String>,
    /// When the publisher's authority ends, in Unix seconds.
    pub valid_until: Option<u64>,
}

/// Called with `(iss, kid)` when a token names a key this server does not
/// hold: the publisher on the same node refreshes that issuer.
pub(crate) type KidNotice = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The issuer keys a server holds, swapped whole on every rebuild.
pub(crate) struct IssuerKeys {
    sets: arc_swap::ArcSwap<BTreeMap<String, Arc<Accepted>>>,
    generation: tokio::sync::watch::Sender<u64>,
    notice: std::sync::RwLock<Option<KidNotice>>,
}

impl std::fmt::Debug for IssuerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuerKeys")
            .field("sets", &self.sets.load())
            .finish_non_exhaustive()
    }
}

impl Default for IssuerKeys {
    fn default() -> Self {
        Self {
            sets: arc_swap::ArcSwap::default(),
            generation: tokio::sync::watch::Sender::new(0),
            notice: std::sync::RwLock::new(None),
        }
    }
}

impl IssuerKeys {
    /// The key `kid` of `iss` at `now`, while its publisher's authority holds
    /// and the set is at most `max_age` seconds old.
    pub(crate) fn key(&self, iss: &str, kid: &str, now: u64, max_age: u64) -> Option<ProviderKey> {
        let sets = self.sets.load();
        let set = sets.get(iss)?;
        if set.valid_until.is_some_and(|until| now >= until) {
            return None;
        }
        if now.saturating_sub(set.fetched_at) > max_age {
            return None;
        }
        set.keys.iter().find(|key| key.kid() == kid).cloned()
    }

    /// Route unknown-`kid` notices to `notice` (the node's publisher).
    #[cfg_attr(not(feature = "identity-publisher"), allow(dead_code))]
    pub(crate) fn set_kid_notice(&self, notice: KidNotice) {
        *self.notice.write().unwrap_or_else(|e| e.into_inner()) = Some(notice);
    }

    /// A token named `kid` of `iss`, which no held (fresh) set has.
    pub(crate) fn notice_unknown_kid(&self, iss: &str, kid: &str) {
        let notice = self
            .notice
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(notice) = notice {
            notice(iss, kid);
        }
    }

    #[cfg_attr(not(all(feature = "enrollment", feature = "docs")), allow(dead_code))]
    fn replace(&self, sets: BTreeMap<String, Arc<Accepted>>) {
        self.sets.store(Arc::new(sets));
        self.generation.send_modify(|generation| *generation += 1);
    }
}

/// A read-only view of a running server's issuer keys (from
/// [`AsterServer::issuer_keys`](super::AsterServer::issuer_keys)).
#[derive(Clone, Debug)]
pub struct IssuerKeysHandle(pub(crate) Arc<IssuerKeys>);

impl IssuerKeysHandle {
    /// Every issuer this server holds keys for, trusted or not.
    pub fn key_sets(&self) -> Vec<IssuerKeySet> {
        self.0
            .sets
            .load()
            .iter()
            .map(|(iss, set)| IssuerKeySet {
                iss: iss.clone(),
                publisher: hex::encode(set.publisher),
                source_url: set.source_url.clone(),
                fetched_at: set.fetched_at,
                kids: set.keys.iter().map(|key| key.kid().to_string()).collect(),
                valid_until: set.valid_until,
            })
            .collect()
    }

    /// The set held for `iss`.
    pub fn key_set(&self, iss: &str) -> Option<IssuerKeySet> {
        self.key_sets().into_iter().find(|set| set.iss == iss)
    }

    /// A receiver that changes on every rebuild, for condition waits:
    /// subscribe, check, then `changed().await` and check again.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.0.generation.subscribe()
    }
}

// ── Reading the namespace ───────────────────────────────────────────────────

#[cfg(all(feature = "enrollment", feature = "docs"))]
pub(crate) use watch::{watch, IssuerKeysWatch};

#[cfg(all(feature = "enrollment", feature = "docs"))]
mod watch {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use aster_transport_core::delegation::issuer_keys::{decode_key_set, ISSUER_KEYS_PREFIX};

    use super::{Accepted, IssuerKeys, IDENTITY_PUBLISHER_ROLE, MAX_KEY_SET_ENTRIES};
    use crate::docs::{Doc, DocEvent};
    use crate::enrollment::runtime::RoleMapping;
    use crate::enrollment::Enrollment;
    use crate::log_targets::IDENTITY;
    use crate::rpc::identity::LiveIdentity;
    use crate::{Node, Result};

    /// Rebuild at least this often, for expiries and revocations applied
    /// after their entry arrived.
    const REBUILD_INTERVAL: Duration = Duration::from_secs(60);
    /// The enrollment records whose change can change a publisher's standing.
    const ENROLLMENT_PREFIX: &[u8] = b"aster/enrollment/v1/";

    /// The watcher task; dropping it stops the updates (the keys stay as
    /// they were).
    pub(crate) struct IssuerKeysWatch {
        task: tokio::task::JoinHandle<()>,
    }

    /// Open (read-only unless held writable) `namespace` and subscribe to it.
    async fn open(node: &Node, namespace: [u8; 32]) -> Result<(Doc, crate::docs::DocEvents)> {
        let doc = node
            .docs()
            .open_or_import_read_namespace(crate::NamespaceId::from_bytes(namespace))
            .await?;
        let events = doc.subscribe().await?;
        Ok((doc, events))
    }

    impl Drop for IssuerKeysWatch {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct Scanner {
        doc: Doc,
        enrollment: Enrollment,
        mapping: RoleMapping,
        identity: Arc<LiveIdentity>,
        keys: Arc<IssuerKeys>,
    }

    /// Fill `keys` from `node`'s copy of `enrollment`'s fleet namespace now,
    /// then keep it up to date with every change.
    pub(crate) async fn watch(
        node: &Node,
        enrollment: Enrollment,
        mapping: RoleMapping,
        identity: Arc<LiveIdentity>,
        keys: Arc<IssuerKeys>,
    ) -> Result<IssuerKeysWatch> {
        // Subscribed before the first read, so no rotation falls between.
        let mut roots = enrollment.root_changes();
        let mut namespace = enrollment.fleet_namespace();
        // Subscribed before the first scan, so no change falls between them.
        let (doc, mut events) = open(node, namespace).await?;
        let mut scanner = Scanner {
            doc,
            enrollment,
            mapping,
            identity,
            keys,
        };
        scanner.rebuild().await;
        let node = node.clone();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(REBUILD_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                let rebuild = tokio::select! {
                    event = events.recv() => match event {
                        Ok(Some(event)) => relevant(&event),
                        // Lagged or failed: rebuild from what is stored.
                        Err(_) => true,
                        Ok(None) => break,
                    },
                    changed = roots.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        true
                    }
                    _ = interval.tick() => true,
                };
                // A root rotation moves the fleet namespace: re-open on the
                // new one (retried every tick until it opens). Until then the
                // rebuild below reads the old namespace under the new root's
                // trust, where no publisher's credential verifies.
                let current = scanner.enrollment.fleet_namespace();
                if current != namespace {
                    match open(&node, current).await {
                        Ok((doc, reopened)) => {
                            tracing::info!(
                                target: IDENTITY,
                                fleet = hex::encode(current).as_str(),
                                "issuer keys: root rotated; watching the new fleet namespace"
                            );
                            scanner.doc = doc;
                            events = reopened;
                            namespace = current;
                        }
                        Err(error) => tracing::warn!(
                            target: IDENTITY,
                            error = %error,
                            "issuer keys: opening the rotated fleet namespace failed"
                        ),
                    }
                }
                if rebuild {
                    scanner.rebuild().await;
                }
            }
        });
        Ok(IssuerKeysWatch { task })
    }

    fn relevant(event: &DocEvent) -> bool {
        match event {
            DocEvent::InsertLocal { entry } | DocEvent::InsertRemote { entry, .. } => {
                entry.key.starts_with(ISSUER_KEYS_PREFIX.as_bytes())
                    || entry.key.starts_with(ENROLLMENT_PREFIX)
            }
            DocEvent::ContentReady { .. } | DocEvent::PendingContentReady => true,
            _ => false,
        }
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    impl Scanner {
        async fn rebuild(&self) {
            let now = unix_now();
            let leeway = self.identity.load().clock_leeway_secs;
            let entries = match self
                .doc
                .query_key_prefix(
                    ISSUER_KEYS_PREFIX.as_bytes().to_vec(),
                    Some(MAX_KEY_SET_ENTRIES as u64 + 1),
                )
                .await
            {
                Ok(entries) => entries,
                Err(error) => {
                    // A local storage failure: keep what was accepted.
                    tracing::warn!(target: IDENTITY, error = %error, "reading issuer keys failed");
                    return;
                }
            };
            if entries.len() > MAX_KEY_SET_ENTRIES {
                tracing::warn!(
                    target: IDENTITY,
                    limit = MAX_KEY_SET_ENTRIES,
                    "too many issuer key-set entries; holding none"
                );
                self.keys.replace(BTreeMap::new());
                return;
            }
            let mut best: BTreeMap<String, Arc<Accepted>> = BTreeMap::new();
            for entry in entries {
                if entry.content_len == 0
                    || entry.content_len > super::MAX_KEY_SET_RECORD_BYTES as u64
                {
                    continue;
                }
                let Some(publisher) = parse_hex32(entry.author.as_str()) else {
                    continue;
                };
                let publisher_id = hex::encode(publisher);
                let bytes = match self
                    .doc
                    .read_entry_contents_if_complete(std::slice::from_ref(&entry.content_hash))
                    .await
                {
                    Ok(mut contents) => match contents.pop().flatten() {
                        Some(bytes) => bytes,
                        // Not arrived yet: ContentReady rebuilds.
                        None => continue,
                    },
                    Err(_) => continue,
                };
                let record = match decode_key_set(&entry.key, &bytes, now, leeway) {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::debug!(
                            target: IDENTITY,
                            publisher = publisher_id.as_str(),
                            reason = %error,
                            "issuer key set ignored"
                        );
                        continue;
                    }
                };
                let ignored = |reason: &str| {
                    tracing::debug!(
                        target: IDENTITY,
                        iss = record.iss.as_str(),
                        publisher = publisher_id.as_str(),
                        reason,
                        "issuer key set ignored"
                    );
                };
                let Some(evidence) = self
                    .enrollment
                    .evidence_for(&publisher, record.credential.as_deref())
                else {
                    ignored("author holds no valid credential or registration");
                    continue;
                };
                if evidence.expires_at().is_some_and(|exp| now >= exp) {
                    ignored("author's credential expired");
                    continue;
                }
                if !self
                    .mapping
                    .roles(evidence.claims())
                    .contains(IDENTITY_PUBLISHER_ROLE)
                {
                    ignored("author lacks aster.identity-publisher");
                    continue;
                }
                let candidate = Accepted {
                    keys: record.keys,
                    publisher,
                    source_url: record.source_url,
                    fetched_at: record.fetched_at,
                    valid_until: evidence.expires_at(),
                };
                let newer = best.get(&record.iss).is_none_or(|held| {
                    (candidate.fetched_at, candidate.publisher) > (held.fetched_at, held.publisher)
                });
                if newer {
                    best.insert(record.iss, Arc::new(candidate));
                }
            }
            for (iss, set) in &best {
                tracing::debug!(
                    target: IDENTITY,
                    iss = iss.as_str(),
                    publisher = hex::encode(set.publisher).as_str(),
                    keys = set.keys.len(),
                    fetched_at = set.fetched_at,
                    "issuer keys held"
                );
            }
            self.keys.replace(best);
        }
    }

    fn parse_hex32(value: &str) -> Option<[u8; 32]> {
        if value.len() != 64 {
            return None;
        }
        hex::decode(value).ok()?.try_into().ok()
    }
}

// ── Writing the namespace (the publisher) ───────────────────────────────────

/// One issuer's key set, as a publisher fetched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuerKeyPublication {
    /// The issuer (`iss`), exactly as in its tokens.
    pub iss: String,
    /// Where the JWK set was fetched (over validated TLS).
    pub source_url: String,
    /// The JWK set as served: `{"keys": [...]}`. Only usable public signature
    /// keys are kept.
    pub jwks_json: String,
    /// When it was fetched, in Unix seconds.
    pub fetched_at: u64,
}

/// Largest JWK set [`publish_issuer_keys`] parses.
pub const MAX_JWKS_JSON_BYTES: usize = 1024 * 1024;

/// Write `publication` into `fleet` (the fleet namespace, opened with write
/// access) as `node`'s record, carrying `credential` (this node's enrollment
/// credential chain; `None` for a root-registered node). Cores accept it when
/// that credential grants [`IDENTITY_PUBLISHER_ROLE`].
///
/// With an `AsterServer`/`AsterClient`, `credential` is
/// `enrollment.credential().map(|(chain, _)| chain)`.
#[cfg(all(feature = "docs", feature = "delegation"))]
pub async fn publish_issuer_keys(
    node: &crate::Node,
    fleet: &crate::Doc,
    credential: Option<&str>,
    publication: &IssuerKeyPublication,
) -> crate::Result<crate::Hash> {
    use aster_transport_core::delegation::issuer_keys::encode_key_set;
    let invalid = |message: String| crate::Error::InvalidArgument(message);
    if publication.jwks_json.len() > MAX_JWKS_JSON_BYTES {
        return Err(invalid(format!(
            "issuer keys: the JWK set is larger than {MAX_JWKS_JSON_BYTES} bytes"
        )));
    }
    let jwks: serde_json::Value = serde_json::from_str(&publication.jwks_json)
        .map_err(|e| invalid(format!("issuer keys: the JWK set is not JSON: {e}")))?;
    let bytes = encode_key_set(
        &publication.iss,
        &publication.source_url,
        publication.fetched_at,
        &jwks,
        credential,
    )
    .map_err(|e| invalid(e.to_string()))?;
    let author = node.docs().default_author().await?;
    // The record's authority is its author: it must be this node's key.
    if author.as_str() != node.id().as_str() {
        return Err(invalid(
            "issuer keys: this node's docs author is not its node id".into(),
        ));
    }
    fleet
        .set_bytes(&author, issuer_keys_key(&publication.iss), bytes)
        .await
}
