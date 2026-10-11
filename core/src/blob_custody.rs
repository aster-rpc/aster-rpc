//! Temporary GC custody and verified named retention, without anonymous tags.
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::api::TempTag;
use iroh_blobs::{BlobFormat, Hash, HashAndFormat};

use crate::{CoreBlobsClient, CoreFetchReport, CoreFetchStrategy};

/// Stable failures for custody/verified-import adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreBlobCustodyError {
    Closed,
    Cancelled,
    HashMismatch { expected: String, actual: String },
    UnprotectedRoot,
}
impl std::fmt::Display for CoreBlobCustodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("blob custody scope is closed"),
            Self::Cancelled => f.write_str("blob operation cancelled before retention commit"),
            Self::HashMismatch { expected, actual } => write!(
                f,
                "blob hash mismatch: expected {expected}, computed {actual}"
            ),
            Self::UnprotectedRoot => {
                f.write_str("root and format are not protected by this custody scope")
            }
        }
    }
}
impl std::error::Error for CoreBlobCustodyError {}

fn format(value: &str) -> Result<BlobFormat> {
    match value {
        "raw" => Ok(BlobFormat::Raw),
        "hash_seq" => Ok(BlobFormat::HashSeq),
        _ => Err(anyhow!("format must be raw or hash_seq")),
    }
}
fn root(hash: &str, fmt: &str) -> Result<HashAndFormat> {
    Ok(HashAndFormat {
        hash: hash.parse()?,
        format: format(fmt)?,
    })
}
fn check_cancel(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(CoreBlobCustodyError::Cancelled.into())
    } else {
        Ok(())
    }
}

struct CancelOnDrop(Option<Arc<AtomicBool>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(flag) = self.0.take() {
            flag.store(true, Ordering::Release);
        }
    }
}

/// Keep native import/commit ownership when its caller drops the wait future.
/// Before commit, cancellation discards the temporary result. Once tag.set is
/// submitted, the worker joins it and reports its definitive result instead.
async fn owned<T: Send + 'static>(
    cancelled: Arc<AtomicBool>,
    work: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let mut cancel_on_drop = CancelOnDrop(Some(cancelled));
    let result = tokio::spawn(work)
        .await
        .map_err(|e| anyhow!("blob worker failed: {e}"))?;
    cancel_on_drop.0.take();
    result
}

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
    guards: Vec<TempTag>,
}
struct Inner {
    client: CoreBlobsClient,
    state: Mutex<State>,
    idle: tokio::sync::Notify,
}

/// An owned in-memory scope for multiple Raw/HashSeq roots. No persistent tag is
/// created until retain. Close rejects new leases; accepted work keeps every
/// guard alive until its last lease finishes. Dropping all owners releases it.
#[derive(Clone)]
pub struct CoreBlobCustody {
    inner: Arc<Inner>,
}

struct LeaseInner {
    owner: Arc<Inner>,
}
impl Drop for LeaseInner {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        if state.closed && state.active == 0 {
            state.guards.clear();
        }
        if state.active == 0 {
            self.owner.idle.notify_waiters();
        }
    }
}

/// Synchronously admitted native work. Clones share one lease and release its
/// protection only after their final owner drops. Operations accepted before
/// close may finish and acquire additional guards without reopening admission.
#[derive(Clone)]
pub struct CoreBlobCustodyLease {
    inner: Arc<LeaseInner>,
}

impl CoreBlobCustody {
    pub fn lease(&self) -> Result<CoreBlobCustodyLease> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(CoreBlobCustodyError::Closed.into());
        }
        state.active += 1;
        Ok(CoreBlobCustodyLease {
            inner: Arc::new(LeaseInner {
                owner: self.inner.clone(),
            }),
        })
    }
    pub fn close(&self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        if state.active == 0 {
            state.guards.clear();
        }
    }
    /// Close admission and await all previously accepted work/guard release.
    /// Cancelling this wait does not cancel or release its active leases.
    pub async fn close_and_wait(&self) {
        self.close();
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
                == 0
            {
                return;
            }
            notified.await;
        }
    }
    pub async fn protect(&self, hash_hex: String, format: String) -> Result<()> {
        self.lease()?.protect(hash_hex, format).await
    }
    pub async fn add_bytes(&self, data: Vec<u8>, format: String) -> Result<String> {
        self.lease()?.add_bytes(data, format).await
    }
    pub async fn add_path(&self, path: String, format: String) -> Result<String> {
        self.lease()?.add_path(path, format).await
    }
    pub async fn retain(
        &self,
        hash_hex: String,
        format: String,
        tag_name: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        self.lease()?
            .retain(hash_hex, format, tag_name, cancelled)
            .await
    }
    pub async fn read(&self, hash_hex: String) -> Result<Vec<u8>> {
        self.lease()?.read(hash_hex).await
    }
    pub async fn download(
        &self,
        hash_hex: String,
        format: String,
        providers: Vec<String>,
        strategy: CoreFetchStrategy,
    ) -> Result<CoreFetchReport> {
        self.lease()?
            .download(hash_hex, format, providers, strategy)
            .await
    }
}

impl CoreBlobCustodyLease {
    pub fn client(&self) -> CoreBlobsClient {
        self.inner.owner.client.clone()
    }
    fn remember(&self, guard: TempTag) {
        self.inner
            .owner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .guards
            .push(guard);
    }
    /// Acknowledge only after the fork expands HashSeq children into any live GC
    /// sweep's protection. Absent roots may be pinned before downloading them.
    pub async fn protect(&self, hash_hex: String, format: String) -> Result<()> {
        self.protect_cancellable(hash_hex, format, None).await
    }
    pub async fn protect_cancellable(
        &self,
        hash_hex: String,
        fmt: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        let value = root(&hash_hex, &fmt)?;
        let lease = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            check_cancel(&flag)?;
            let guard = lease.client().store.tags().temp_tag(value).await?;
            check_cancel(&flag)?;
            lease.remember(guard);
            Ok(())
        })
        .await
    }
    pub async fn add_bytes(&self, data: Vec<u8>, fmt: String) -> Result<String> {
        self.add_bytes_cancellable(data, fmt, None).await
    }
    pub async fn add_bytes_cancellable(
        &self,
        data: Vec<u8>,
        fmt: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<String> {
        let fmt = format(&fmt)?;
        let lease = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            check_cancel(&flag)?;
            let guard = lease
                .client()
                .store
                .blobs()
                .add_bytes_with_opts((data, fmt))
                .temp_tag()
                .await?;
            check_cancel(&flag)?;
            let hash = guard.hash().to_hex().to_string();
            lease.remember(guard);
            Ok(hash)
        })
        .await
    }
    pub async fn add_path(&self, path: String, fmt: String) -> Result<String> {
        self.add_path_cancellable(path, fmt, None).await
    }
    pub async fn add_path_cancellable(
        &self,
        path: String,
        fmt: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<String> {
        let fmt = format(&fmt)?;
        let lease = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            check_cancel(&flag)?;
            let guard = lease
                .client()
                .store
                .blobs()
                .add_path_with_opts(AddPathOptions {
                    path: path.into(),
                    mode: ImportMode::Copy,
                    format: fmt,
                })
                .temp_tag()
                .await?;
            check_cancel(&flag)?;
            let hash = guard.hash().to_hex().to_string();
            lease.remember(guard);
            Ok(hash)
        })
        .await
    }
    /// Commit only a root with the exact format held by this scope. Replaces
    /// existing tags using normal store semantics; this adds no fsync guarantee.
    pub async fn retain(
        &self,
        hash_hex: String,
        fmt: String,
        tag_name: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        let value = root(&hash_hex, &fmt)?;
        if tag_name.is_empty() {
            return Err(anyhow!("tag name must be nonempty"));
        }
        let lease = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            if !lease
                .inner
                .owner
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .guards
                .iter()
                .any(|guard| guard.hash_and_format() == value)
            {
                return Err(CoreBlobCustodyError::UnprotectedRoot.into());
            }
            check_cancel(&flag)?;
            // This await is owned: dropping the caller cannot drop its guard
            // while the store's already-enqueued commit is still running.
            lease
                .client()
                .store
                .tags()
                .set(tag_name.as_bytes(), value)
                .await?;
            Ok(())
        })
        .await
    }
    /// Return root bytes without Collection interpretation.
    pub async fn read(&self, hash_hex: String) -> Result<Vec<u8>> {
        self.read_cancellable(hash_hex, None).await
    }
    pub async fn read_cancellable(
        &self,
        hash_hex: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<Vec<u8>> {
        let lease = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            lease
                .protect_cancellable(hash_hex.clone(), "raw".into(), Some(flag.clone()))
                .await?;
            let bytes = lease.client().read_to_bytes(hash_hex).await?;
            check_cancel(&flag)?;
            Ok(bytes)
        })
        .await
    }
    /// Fetch to the store without imposing a Collection schema on HashSeq.
    /// Cancellation drops the request then waits for native downloader drain
    /// with custody retained; other requests may extend that drain.
    pub async fn download(
        &self,
        hash_hex: String,
        fmt: String,
        providers: Vec<String>,
        strategy: CoreFetchStrategy,
    ) -> Result<CoreFetchReport> {
        self.download_cancellable(hash_hex, fmt, providers, strategy, None)
            .await
    }
    pub async fn download_cancellable(
        &self,
        hash_hex: String,
        fmt: String,
        providers: Vec<String>,
        strategy: CoreFetchStrategy,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<CoreFetchReport> {
        let lease = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            lease
                .protect_cancellable(hash_hex.clone(), fmt.clone(), Some(flag.clone()))
                .await?;
            let client = lease.client();
            let mut request = Box::pin(client.download_hash_to_store_multi(
                hash_hex.clone(),
                providers,
                fmt.clone(),
                strategy,
            ));
            let result = tokio::select! {
                result = &mut request => Some(result),
                () = cancellation_requested(&flag) => None,
            };
            drop(request);
            let Some(result) = result else {
                // The fork's whole-fetch actor is not immediately cancellation
                // aware during dial/header awaits. Never release its custody
                // merely because its progress receiver has been dropped.
                client.downloader.wait_idle().await?;
                return Err(CoreBlobCustodyError::Cancelled.into());
            };
            let report = result?;
            // Expand now-resident HashSeq children before acknowledging success.
            lease
                .protect_cancellable(hash_hex, fmt, Some(flag.clone()))
                .await?;
            Ok(report)
        })
        .await
    }
}

impl CoreBlobsClient {
    pub fn custody(&self) -> CoreBlobCustody {
        CoreBlobCustody {
            inner: Arc::new(Inner {
                client: self.clone(),
                state: Mutex::new(State::default()),
                idle: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Import without an anonymous root, verify the store-computed hash, then
    /// commit the exact named root. Cancellation is checked before commit;
    /// once submitted, commit is joined and its actual outcome wins.
    pub async fn add_bytes_verified(
        &self,
        data: Vec<u8>,
        expected_hash: String,
        fmt: String,
        tag_name: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<String> {
        let expected = root(&expected_hash, &fmt)?;
        if tag_name.is_empty() {
            return Err(anyhow!("tag name must be nonempty"));
        }
        let client = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            check_cancel(&flag)?;
            let guard = client
                .store
                .blobs()
                .add_bytes_with_opts((data, expected.format))
                .temp_tag()
                .await?;
            check_cancel(&flag)?;
            commit_verified(&client, guard, expected.hash, tag_name, &flag).await
        })
        .await
    }
    /// Verified retained import using Copy, never a mutable source reference.
    pub async fn add_path_verified(
        &self,
        path: String,
        expected_hash: String,
        fmt: String,
        tag_name: String,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<String> {
        let expected = root(&expected_hash, &fmt)?;
        if tag_name.is_empty() {
            return Err(anyhow!("tag name must be nonempty"));
        }
        let client = self.clone();
        let cancelled = cancelled.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let flag = cancelled.clone();
        owned(cancelled, async move {
            check_cancel(&flag)?;
            let guard = client
                .store
                .blobs()
                .add_path_with_opts(AddPathOptions {
                    path: path.into(),
                    mode: ImportMode::Copy,
                    format: expected.format,
                })
                .temp_tag()
                .await?;
            check_cancel(&flag)?;
            commit_verified(&client, guard, expected.hash, tag_name, &flag).await
        })
        .await
    }
}

async fn commit_verified(
    client: &CoreBlobsClient,
    guard: TempTag,
    expected: Hash,
    tag: String,
    cancelled: &AtomicBool,
) -> Result<String> {
    let value = guard.hash_and_format();
    if value.hash != expected {
        return Err(CoreBlobCustodyError::HashMismatch {
            expected: expected.to_hex().to_string(),
            actual: value.hash.to_hex().to_string(),
        }
        .into());
    }
    check_cancel(cancelled)?;
    client.store.tags().set(tag.as_bytes(), value).await?;
    // Both persistent mutation and fork custody expansion have acknowledged.
    drop(guard);
    Ok(value.hash.to_hex().to_string())
}

async fn cancellation_requested(flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CoreEndpointConfig, CoreNode};

    async fn client() -> (CoreNode, CoreBlobsClient) {
        let node = CoreNode::memory_with_alpns_and_gc(
            Vec::new(),
            Some(CoreEndpointConfig {
                relay_mode: Some("disabled".into()),
                ..Default::default()
            }),
            Some(std::time::Duration::from_secs(3600)),
        )
        .await
        .unwrap();
        let client = node.blobs_client();
        (node, client)
    }

    #[tokio::test]
    async fn abandoned_wait_requests_cancel_and_keeps_guard_until_worker_cleanup() {
        let (node, client) = client().await;
        let expected = Hash::new(b"cancel before commit");
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = flag.clone();
        let worker_client = client.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(owned(flag.clone(), async move {
            let guard = worker_client
                .store
                .blobs()
                .add_slice(b"cancel before commit")
                .temp_tag()
                .await?;
            ready_tx.send(()).unwrap();
            go_rx.await.unwrap();
            let result = commit_verified(
                &worker_client,
                guard,
                expected,
                "must-not-commit".into(),
                &worker_flag,
            )
            .await;
            done_tx
                .send(matches!(
                    result
                        .as_ref()
                        .err()
                        .and_then(|e| e.downcast_ref::<CoreBlobCustodyError>()),
                    Some(CoreBlobCustodyError::Cancelled)
                ))
                .unwrap();
            result
        }));
        ready_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(flag.load(Ordering::Acquire));
        client.gc_run_once().await.unwrap();
        assert!(client
            .blob_has(expected.to_hex().to_string())
            .await
            .unwrap());
        go_tx.send(()).unwrap();
        assert!(done_rx.await.unwrap());
        client.gc_run_once().await.unwrap();
        assert!(!client
            .blob_has(expected.to_hex().to_string())
            .await
            .unwrap());
        assert!(client.tag_list().await.unwrap().is_empty());
        node.close().await;
    }

    #[tokio::test]
    async fn cancellation_after_commit_decision_returns_committed_success() {
        let (node, client) = client().await;
        let expected = Hash::new(b"commit wins");
        let flag = Arc::new(AtomicBool::new(false));
        let worker_flag = flag.clone();
        let worker_client = client.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(owned(flag.clone(), async move {
            let guard = worker_client
                .store
                .blobs()
                .add_slice(b"commit wins")
                .temp_tag()
                .await?;
            // This is the same last cancellation check as the commit helper.
            check_cancel(&worker_flag)?;
            ready_tx.send(()).unwrap();
            go_rx.await.unwrap();
            worker_client
                .store
                .tags()
                .set(b"committed", guard.hash_and_format())
                .await?;
            drop(guard);
            Ok(expected.to_hex().to_string())
        }));
        ready_rx.await.unwrap();
        flag.store(true, Ordering::Release);
        go_tx.send(()).unwrap();
        assert_eq!(
            waiter.await.unwrap().unwrap(),
            expected.to_hex().to_string()
        );
        assert_eq!(
            client
                .tag_get("committed".into())
                .await
                .unwrap()
                .unwrap()
                .hash,
            expected.to_hex().to_string()
        );
        client.gc_run_once().await.unwrap();
        assert!(client
            .blob_has(expected.to_hex().to_string())
            .await
            .unwrap());
        node.close().await;
    }
}
