//! Topology extension points implemented by upcalling: the read-time
//! admission filter ([`AdmittedFn`]) and, for the placement manager, a
//! foreign topology snapshot source.
//!
//! ## Cost of a foreign `admitted` filter
//!
//! `AdmittedFn` is synchronous and sits on the topology **read path**: every
//! derivation of the shared view (at most once a second, the view cache)
//! calls it once per topology record author and once per live vertex. One
//! upcall per call would put a foreign round trip -- milliseconds, under the
//! binding's event loop -- inside every read. So [`ForeignAdmittedFn`]
//! answers from a local cache and never blocks:
//!
//! - **hit** (fresh entry): one `RwLock` read + hash lookup;
//! - **stale** entry (older than `ttl`): the cached answer, and the id is
//!   queued for refresh (stale-while-revalidate);
//! - **miss**: the [`UpcallKind::TopologyAdmitted`] failure-mode answer
//!   (fail-closed by default: the author is filtered), and the id is queued.
//!
//! One background task drains the queue into **batched** upcalls (up to
//! `max_batch` ids each). A newly seen author therefore counts from the
//! first view derived after its batch is answered (typically the next view,
//! ~1 s). A failed batch caches the fallback for `failure_retry` so a dead
//! binding is not hammered. When admission is revoked, call
//! [`ForeignAdmittedFn::set`] / [`invalidate`](ForeignAdmittedFn::invalidate)
//! to make it take effect at the next view instead of after `ttl`; warm the
//! cache with [`prefetch`](ForeignAdmittedFn::prefetch) before
//! `enable_shared` to avoid the initial fallback window.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::codec::{self, TopologyViewParts};
use super::{UpcallError, UpcallKind, Upcalls};
use crate::log_targets::UPCALL;
use crate::topology::{AdmittedFn, TopologySnapshot};

/// Tuning for [`ForeignAdmittedFn`].
#[derive(Clone, Copy, Debug)]
pub struct AdmittedCacheOptions {
    /// How long an answer is served without a refresh. Default 30 s.
    pub ttl: Duration,
    /// How long a failed batch's fallback answer is cached. Default 1 s.
    pub failure_retry: Duration,
    /// Most ids per upcall. Default 256.
    pub max_batch: usize,
}

impl Default for AdmittedCacheOptions {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(30),
            failure_retry: Duration::from_secs(1),
            max_batch: 256,
        }
    }
}

/// Counters for [`ForeignAdmittedFn`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdmittedCacheStats {
    /// Lookups answered from a fresh entry.
    pub hits: u64,
    /// Lookups answered from a stale entry (refresh queued).
    pub stale: u64,
    /// Lookups with no entry (fallback answer, resolution queued).
    pub misses: u64,
    /// Batched upcalls issued.
    pub upcalls: u64,
}

struct Entry {
    admitted: bool,
    expires: Instant,
}

struct AdmitInner {
    upcalls: Upcalls,
    opts: AdmittedCacheOptions,
    cache: RwLock<HashMap<String, Entry>>,
    queued: Mutex<BTreeSet<String>>,
    wake: Arc<Notify>,
    hits: AtomicU64,
    stale: AtomicU64,
    misses: AtomicU64,
    upcalls_issued: AtomicU64,
}

impl Drop for AdmitInner {
    fn drop(&mut self) {
        // Let the refresher notice the last handle is gone.
        self.wake.notify_one();
    }
}

/// A cached, batched foreign topology admission filter. Clone-cheap.
#[derive(Clone)]
pub struct ForeignAdmittedFn {
    inner: Arc<AdmitInner>,
}

impl std::fmt::Debug for ForeignAdmittedFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignAdmittedFn")
            .field("stats", &self.stats())
            .finish()
    }
}

impl ForeignAdmittedFn {
    /// Build the filter and spawn its refresher on the current tokio
    /// runtime. The refresher exits when the last handle (including every
    /// [`admitted_fn`](Self::admitted_fn) closure) is dropped.
    ///
    /// # Panics
    /// Outside a tokio runtime.
    pub fn new(upcalls: Upcalls, opts: AdmittedCacheOptions) -> Self {
        let wake = Arc::new(Notify::new());
        let inner = Arc::new(AdmitInner {
            upcalls,
            opts: AdmittedCacheOptions {
                max_batch: opts.max_batch.max(1),
                ..opts
            },
            cache: RwLock::new(HashMap::new()),
            queued: Mutex::new(BTreeSet::new()),
            wake: wake.clone(),
            hits: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            upcalls_issued: AtomicU64::new(0),
        });
        tokio::spawn(refresher(Arc::downgrade(&inner), wake));
        Self { inner }
    }

    /// The synchronous filter for `TopologySwarmOptions::admitted`.
    pub fn admitted_fn(&self) -> AdmittedFn {
        let inner = self.inner.clone();
        Arc::new(move |node: &str| lookup(&inner, node))
    }

    /// Look one id up exactly as the filter does.
    pub fn is_admitted(&self, node_hex: &str) -> bool {
        lookup(&self.inner, node_hex)
    }

    /// Resolve `ids` now with one upcall (awaiting it) and cache the
    /// answers.
    pub async fn prefetch(&self, ids: &[String]) -> Result<(), UpcallError> {
        if ids.is_empty() {
            return Ok(());
        }
        resolve(&self.inner, ids.to_vec()).await
    }

    /// Push an answer from the binding (e.g. on revocation); it is served
    /// for `ttl`.
    pub fn set(&self, node_hex: &str, admitted: bool) {
        let expires = Instant::now() + self.inner.opts.ttl;
        self.inner
            .cache
            .write()
            .unwrap()
            .insert(node_hex.to_string(), Entry { admitted, expires });
    }

    /// Forget one id; the next lookup misses and re-resolves.
    pub fn invalidate(&self, node_hex: &str) {
        self.inner.cache.write().unwrap().remove(node_hex);
    }

    /// Forget every id.
    pub fn clear(&self) {
        self.inner.cache.write().unwrap().clear();
    }

    pub fn stats(&self) -> AdmittedCacheStats {
        AdmittedCacheStats {
            hits: self.inner.hits.load(Ordering::Relaxed),
            stale: self.inner.stale.load(Ordering::Relaxed),
            misses: self.inner.misses.load(Ordering::Relaxed),
            upcalls: self.inner.upcalls_issued.load(Ordering::Relaxed),
        }
    }
}

fn lookup(inner: &AdmitInner, node: &str) -> bool {
    {
        let cache = inner.cache.read().unwrap();
        if let Some(e) = cache.get(node) {
            if e.expires > Instant::now() {
                inner.hits.fetch_add(1, Ordering::Relaxed);
                return e.admitted;
            }
            inner.stale.fetch_add(1, Ordering::Relaxed);
            let v = e.admitted;
            drop(cache);
            enqueue(inner, node);
            return v;
        }
    }
    inner.misses.fetch_add(1, Ordering::Relaxed);
    enqueue(inner, node);
    inner
        .upcalls
        .policy(UpcallKind::TopologyAdmitted)
        .fallback()
}

fn enqueue(inner: &AdmitInner, node: &str) {
    let fresh = inner.queued.lock().unwrap().insert(node.to_string());
    if fresh {
        inner.wake.notify_one();
    }
}

async fn resolve(inner: &AdmitInner, ids: Vec<String>) -> Result<(), UpcallError> {
    inner.upcalls_issued.fetch_add(1, Ordering::Relaxed);
    let answer = inner
        .upcalls
        .call(
            UpcallKind::TopologyAdmitted,
            codec::encode_topology_admitted_request(&ids),
        )
        .await
        .and_then(|bytes| {
            let v = codec::decode_topology_admitted_response(&bytes)
                .map_err(|e| UpcallError::Decode(e.to_string()))?;
            if v.len() != ids.len() {
                return Err(UpcallError::Decode(format!(
                    "{} answers for {} ids",
                    v.len(),
                    ids.len()
                )));
            }
            Ok(v)
        });
    let now = Instant::now();
    match answer {
        Ok(v) => {
            let mut cache = inner.cache.write().unwrap();
            for (id, admitted) in ids.into_iter().zip(v) {
                cache.insert(
                    id,
                    Entry {
                        admitted,
                        expires: now + inner.opts.ttl,
                    },
                );
            }
            Ok(())
        }
        Err(e) => {
            let fallback = inner
                .upcalls
                .policy(UpcallKind::TopologyAdmitted)
                .fallback();
            tracing::warn!(
                target: UPCALL,
                error = %e,
                ids = ids.len(),
                fallback,
                "topology admitted batch failed; caching the failure-mode answer"
            );
            let mut cache = inner.cache.write().unwrap();
            for id in ids {
                // Keep a still-cached real answer; only fill gaps.
                let entry = cache.entry(id).or_insert(Entry {
                    admitted: fallback,
                    expires: now,
                });
                entry.expires = entry.expires.max(now + inner.opts.failure_retry);
            }
            Err(e)
        }
    }
}

async fn refresher(weak: Weak<AdmitInner>, wake: Arc<Notify>) {
    loop {
        let notified = wake.notified();
        let batch = {
            let Some(inner) = weak.upgrade() else { return };
            let mut q = inner.queued.lock().unwrap();
            let batch: Vec<String> = q.iter().take(inner.opts.max_batch).cloned().collect();
            for id in &batch {
                q.remove(id);
            }
            batch
        };
        if batch.is_empty() {
            notified.await;
            continue;
        }
        let Some(inner) = weak.upgrade() else { return };
        let _ = resolve(&inner, batch).await;
    }
}

/// A placement [`TopologySource`](crate::coordination::client::manager::TopologySource)
/// answered by foreign code (kind [`UpcallKind::TopologySnapshot`]): the
/// binding returns a serialized view ([`TopologyViewParts`]) and Rust builds
/// the immutable [`TopologySnapshot`] from it. A failed upcall is an error,
/// which stops that reconciliation pass.
#[derive(Clone, Debug)]
pub struct ForeignTopologySource {
    upcalls: Upcalls,
}

impl ForeignTopologySource {
    pub fn new(upcalls: Upcalls) -> Self {
        Self { upcalls }
    }

    /// One snapshot, by upcall.
    pub async fn fetch(&self) -> Result<TopologySnapshot, UpcallError> {
        let bytes = self
            .upcalls
            .call(
                UpcallKind::TopologySnapshot,
                codec::encode_topology_snapshot_request(),
            )
            .await?;
        let parts: TopologyViewParts = codec::decode_topology_snapshot(&bytes)
            .map_err(|e| UpcallError::Decode(e.to_string()))?;
        Ok(parts.into_snapshot())
    }
}

#[cfg(feature = "coordination-client")]
#[async_trait::async_trait]
impl crate::coordination::client::manager::TopologySource for ForeignTopologySource {
    async fn snapshot(&self) -> crate::Result<TopologySnapshot> {
        Ok(self.fetch().await?)
    }
}
