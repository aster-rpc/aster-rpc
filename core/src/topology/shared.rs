//! Topology v2 swarm engine — publishes this node's measurements into the
//! shared topology namespace and derives the swarm-wide cluster view.
//!
//! One `CoreTopoSwarm` per node, created via
//! [`CoreNode::topology_join_swarm`](crate::CoreNode::topology_join_swarm)
//! with the namespace **secret** (distributed out-of-band, normally inside a
//! sealed grant — see `aster::grants`). The engine:
//!
//! - imports the write namespace (idempotent) and starts doc sync with
//!   bootstrap + already-connected peers;
//! - runs a publisher task that heartbeats this node's `position` record
//!   and one `RttEdge` half per measured peer (near *and* far — far
//!   measurements are separation evidence);
//! - answers `view()` by reading the whole `topo/v1/` prefix, enforcing the
//!   reader rules ([`TopoRecords::insert_entry`]), and running the pure
//!   derivation ([`cluster::derive`]).

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Result};
use iroh::Endpoint;

use super::cluster::{self, SharedView, TopoConfig, TopoRecords};
use super::records::{self, NetworkPosition, RttEdge, TOPO_KEY_PREFIX};
use super::{classify_ip, AddrClass};
use crate::{CoreDoc, CoreMonitor};

/// How long a derived [`SharedView`] snapshot is served before recomputing.
const VIEW_CACHE_TTL: Duration = Duration::from_secs(1);
/// Size caps enforced *before* fetching record contents — a hostile
/// writer's oversized values are never read. Positions get headroom for
/// future prefix hashes; edges are a handful of integers.
const MAX_POSITION_LEN: u64 = 4096;
const MAX_EDGE_LEN: u64 = 256;

/// Pluggable admission check: is this author (hex node id) currently an
/// admitted producer?
pub type AdmittedFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Configuration for joining a topology swarm.
#[derive(Clone)]
pub struct CoreTopoSwarmConfig {
    /// Peers (hex node ids) to start doc sync with, in addition to every
    /// peer the connection monitor already knows.
    pub bootstrap: Vec<String>,
    /// Read-time admission filter. `None` = every holder of the namespace
    /// secret counts (the secret itself is admission-gated by how it was
    /// distributed — e.g. sealed grants).
    pub admitted: Option<AdmittedFn>,
    /// Heartbeat cadence for the `position` record and edge halves.
    pub position_refresh: Duration,
    /// Reader timing/coverage rules (design-doc defaults).
    pub topo: TopoConfig,
}

impl Default for CoreTopoSwarmConfig {
    fn default() -> Self {
        Self {
            bootstrap: Vec::new(),
            admitted: None,
            position_refresh: Duration::from_secs(60),
            topo: TopoConfig::default(),
        }
    }
}

impl fmt::Debug for CoreTopoSwarmConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreTopoSwarmConfig")
            .field("bootstrap", &self.bootstrap)
            .field("admitted", &self.admitted.as_ref().map(|_| "<fn>"))
            .field("position_refresh", &self.position_refresh)
            .field("topo", &self.topo)
            .finish()
    }
}

/// A joined topology swarm. Clone-cheap via `Arc` from
/// [`CoreNode::topology_swarm`](crate::CoreNode::topology_swarm).
pub struct CoreTopoSwarm {
    doc: CoreDoc,
    namespace_id: [u8; 32],
    self_hex: String,
    monitor: CoreMonitor,
    endpoint: Endpoint,
    config: CoreTopoSwarmConfig,
    view_cache: Mutex<Option<(Instant, Arc<SharedView>)>>,
    /// Peers we've already offered to the doc sync set.
    synced_peers: Mutex<HashSet<String>>,
}

impl fmt::Debug for CoreTopoSwarm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreTopoSwarm")
            .field("namespace_id", &hex::encode(self.namespace_id))
            .field("self_hex", &self.self_hex)
            .finish()
    }
}

impl CoreTopoSwarm {
    /// Import the namespace, start sync, and spawn the publisher.
    ///
    /// Returns the swarm handle **plus the publisher's abort guard**. The
    /// guard — not the `Arc` — owns the publisher's lifetime: the node
    /// keeps it in its swarm registration slot, so `CoreNode::close()` (or
    /// re-joining) stops the publisher even while external `Arc`s to the
    /// swarm are still held. Those external handles keep only the query
    /// surface alive. The task additionally holds a mere `Weak`, so it
    /// also exits if every handle is gone.
    pub(crate) async fn start(
        doc: CoreDoc,
        namespace_id: [u8; 32],
        self_hex: String,
        monitor: CoreMonitor,
        endpoint: Endpoint,
        config: CoreTopoSwarmConfig,
    ) -> Result<(Arc<Self>, crate::AbortOnDrop)> {
        let swarm = Arc::new(Self {
            doc,
            namespace_id,
            self_hex,
            monitor,
            endpoint,
            config,
            view_cache: Mutex::new(None),
            synced_peers: Mutex::new(HashSet::new()),
        });

        // First publish + sync immediately so joiners appear without
        // waiting a full refresh interval, then heartbeat.
        swarm.publish_once().await;

        let weak = Arc::downgrade(&swarm);
        let refresh = swarm.config.position_refresh;
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(refresh);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // immediate first tick already published
            loop {
                ticker.tick().await;
                let Some(swarm) = weak.upgrade() else { break };
                swarm.publish_once().await;
            }
        });
        let guard = crate::AbortOnDrop(task.abort_handle());

        Ok((swarm, guard))
    }

    /// The topology namespace id (doubles as the swarm id for bridge
    /// scoring).
    pub fn namespace_id(&self) -> [u8; 32] {
        self.namespace_id
    }

    /// One publisher beat: position record, edge halves, sync offers.
    async fn publish_once(&self) {
        let now_ms = unix_now_ms();

        // Offer doc sync to bootstrap + monitor-known peers we haven't
        // *successfully* offered yet (start_sync is additive). Peers are
        // marked synced only after the offer succeeds, so a failed batch is
        // retried next beat; unparseable ids are dropped permanently so one
        // bad bootstrap entry can't poison every batch forever.
        let mut new_peers = Vec::new();
        {
            let mut synced = self.synced_peers.lock().unwrap_or_else(|e| e.into_inner());
            for peer in self
                .config
                .bootstrap
                .iter()
                .cloned()
                .chain(self.monitor.peer_views().into_iter().map(|v| v.node_id))
            {
                if peer == self.self_hex || synced.contains(&peer) || new_peers.contains(&peer) {
                    continue;
                }
                if peer.parse::<iroh::EndpointId>().is_err() {
                    tracing::debug!("topology: dropping unparseable sync peer {peer}");
                    synced.insert(peer); // never retry garbage
                    continue;
                }
                new_peers.push(peer);
            }
        }
        if !new_peers.is_empty() {
            match self.doc.start_sync(new_peers.clone()).await {
                Ok(()) => {
                    let mut synced = self.synced_peers.lock().unwrap_or_else(|e| e.into_inner());
                    synced.extend(new_peers);
                }
                Err(e) => tracing::debug!("topology: start_sync failed (will retry): {e}"),
            }
        }

        // Position record.
        let addr = self.endpoint.addr();
        let observed_public = addr
            .ip_addrs()
            .find(|sa| classify_ip(sa.ip()) == AddrClass::Public)
            .map(|sa| sa.to_string())
            .unwrap_or_default();
        let home_relay = addr
            .relay_urls()
            .next()
            .map(|u| u.to_string())
            .unwrap_or_default();
        let position = NetworkPosition {
            node_id: hex::decode(&self.self_hex).unwrap_or_default(),
            observed_public,
            asn: 0,
            home_relay,
            prefix_hashes: Vec::new(),
            updated_unix_ms: now_ms as i64,
        };
        if let Err(e) = self
            .doc
            .set_bytes(
                self.self_hex.clone(),
                records::position_key(&self.self_hex),
                records::encode_position(&position),
            )
            .await
        {
            tracing::debug!("topology: position publish failed: {e}");
        }

        // One RttEdge half per measured peer — near or far.
        for view in self.monitor.peer_views() {
            let Some(rtt_us) = view.rtt_us else { continue };
            let edge = RttEdge {
                rtt_us: rtt_us.min(i32::MAX as u64) as i32,
                samples: view.samples.min(i32::MAX as u64) as i32,
                held_since_unix_ms: view.cluster_held_since_unix_ms as i64,
                measured_unix_ms: view.last_measured_unix_ms as i64,
            };
            if let Err(e) = self
                .doc
                .set_bytes(
                    self.self_hex.clone(),
                    records::rtt_key(&self.self_hex, &view.node_id),
                    records::encode_rtt_edge(&edge),
                )
                .await
            {
                tracing::debug!("topology: edge publish failed: {e}");
            }
        }
    }

    /// The derived shared view (cached ~1s). Same records + same rules on
    /// every node → same clusters on every node.
    pub async fn view(&self) -> Result<Arc<SharedView>> {
        {
            let cache = self.view_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, view)) = cache.as_ref() {
                if at.elapsed() < VIEW_CACHE_TTL {
                    return Ok(view.clone());
                }
            }
        }

        let now_ms = unix_now_ms();
        let admitted = self.config.admitted.clone();
        let admitted_check = move |n: &str| admitted.as_ref().is_none_or(|f| f(n));

        // A single admitted-but-hostile writer must have *bounded* reader
        // cost, so validation happens before any content is fetched:
        // key-shape + attribution + admission + size caps prefilter the
        // entry list, and the read is two-pass — positions first, then
        // only the RTT edges whose endpoints are live admitted vertices.
        let entries = self
            .doc
            .query_key_prefix(TOPO_KEY_PREFIX.as_bytes().to_vec(), None)
            .await?;

        let mut dropped = 0u64;
        let mut pos_entries = Vec::new();
        let mut edge_entries = Vec::new();
        for entry in &entries {
            let Some(key) = records::parse_key(&entry.key) else {
                continue; // unknown key shape/version — forward compat
            };
            if key.node() != entry.author_id {
                dropped += 1; // forged path — never fetched
                continue;
            }
            if !admitted_check(&entry.author_id) {
                continue; // unadmitted producer — filtered, not anomalous
            }
            match key {
                records::TopoKey::Position { .. } => {
                    if entry.content_len > MAX_POSITION_LEN {
                        dropped += 1;
                        continue;
                    }
                    pos_entries.push(entry);
                }
                records::TopoKey::Rtt { peer, .. } => {
                    if entry.content_len > MAX_EDGE_LEN {
                        dropped += 1;
                        continue;
                    }
                    edge_entries.push((entry, peer));
                }
                // LAN attestations don't participate in derivation yet —
                // don't pay to fetch them.
                records::TopoKey::Lan { .. } => {}
            }
        }

        // Pass 1: positions → the live-admitted vertex set.
        let mut recs = TopoRecords::default();
        let contents = self
            .doc
            .read_entry_contents_if_complete(
                pos_entries.iter().map(|e| e.content_hash.clone()).collect(),
            )
            .await?;
        for (entry, content) in pos_entries.iter().zip(contents) {
            let Some(bytes) = content else { continue }; // value not local yet
            recs.insert_entry(
                &entry.author_id,
                &entry.key,
                &bytes,
                now_ms,
                self.config.topo.max_skew,
            );
        }
        let liveness_ms = self.config.topo.liveness_ttl.as_millis() as i64;
        let live: std::collections::HashSet<String> = recs
            .positions
            .iter()
            .filter(|(_, p)| now_ms as i64 - p.updated_unix_ms <= liveness_ms)
            .map(|(node, _)| node.clone())
            .collect();

        // Pass 2: only edges between live vertices — bogus peer ids in a
        // writer's own rtt/* keys are skipped without a read.
        let wanted: Vec<_> = edge_entries
            .into_iter()
            .filter(|(entry, peer)| live.contains(&entry.author_id) && live.contains(peer))
            .map(|(entry, _)| entry)
            .collect();
        let contents = self
            .doc
            .read_entry_contents_if_complete(
                wanted.iter().map(|e| e.content_hash.clone()).collect(),
            )
            .await?;
        for (entry, content) in wanted.iter().zip(contents) {
            let Some(bytes) = content else { continue };
            recs.insert_entry(
                &entry.author_id,
                &entry.key,
                &bytes,
                now_ms,
                self.config.topo.max_skew,
            );
        }
        recs.dropped += dropped;

        let view = Arc::new(cluster::derive(
            &recs,
            &admitted_check,
            &self.namespace_id,
            now_ms,
            &self.config.topo,
        ));

        *self.view_cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), view.clone()));
        Ok(view)
    }
}

pub(crate) fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Validate a 32-byte secret and derive the namespace id, shared by
/// [`CoreNode::topology_join_swarm`](crate::CoreNode::topology_join_swarm).
pub fn namespace_id_for_secret(secret: &[u8]) -> Result<[u8; 32]> {
    if secret.len() != 32 {
        return Err(anyhow!("topology namespace secret must be 32 bytes"));
    }
    crate::namespace::namespace_secret_id(secret)
}
