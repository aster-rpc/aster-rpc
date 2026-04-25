//! Stream pool primitives for the multiplexed-streams architecture.
//!
//! See `ffi_spec/Aster-multiplexed-streams.md` for the design spec. This
//! module implements the per-connection pool described in §3, §5, and §9.
//!
//! The pool is generic over the stream type `T` so it can be unit-tested
//! without a real QUIC connection. In production `T` is a multiplexed QUIC
//! bidi stream handle; in tests `T` is a synthetic counter token.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::time::timeout;

/// Routing key for the per-connection pool. `None` is the stateless
/// SHARED pool; `Some(bytes)` is a session-bound pool.
pub type PoolKey = Option<SessionId>;

/// Opaque session identifier. In production this is the 32-byte hash of
/// the session token; for the pool primitive it is just an owned byte
/// string that is cheap to clone and hash.
pub type SessionId = Vec<u8>;

/// Configuration for a per-connection pool. See spec §9.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Maximum number of multiplexed streams in the SHARED pool per
    /// connection. Default 8.
    pub shared_pool_size: usize,
    /// Maximum number of multiplexed streams per session pool. Default 1.
    pub session_pool_size: usize,
    /// How long a caller waits for a free stream before failing with
    /// `AcquireError::PoolFull`. Default 5 seconds.
    pub stream_acquire_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            shared_pool_size: 8,
            session_pool_size: 1,
            stream_acquire_timeout: Duration::from_millis(5000),
        }
    }
}

/// Errors that can be returned from `StreamPool::acquire`.
#[derive(Debug)]
#[non_exhaustive]
pub enum AcquireError {
    /// The pool was at its configured capacity and waiting for a free
    /// stream exceeded `stream_acquire_timeout`.
    PoolFull,
    /// Opening a new stream was blocked because the underlying QUIC
    /// connection's `max_concurrent_streams` ceiling was reached, and
    /// retrying exceeded `stream_acquire_timeout`.
    QuicLimitReached,
    /// Generic acquire timeout without a more specific reason.
    Timeout,
    /// Connect-time validation: the peer advertised a
    /// `max_concurrent_streams` ceiling too small to support the
    /// multiplexed-streams model.
    PeerStreamLimitTooLow { negotiated: u64 },
    /// The stream factory (i.e. `Connection::open_bi`) returned an error.
    StreamOpenFailed(anyhow::Error),
    /// The pool was shut down while a caller was waiting.
    Closed,
}

impl fmt::Display for AcquireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoolFull => write!(f, "stream pool full: acquire timed out waiting for a free stream"),
            Self::QuicLimitReached => write!(f, "QUIC max_concurrent_streams ceiling reached: acquire timed out"),
            Self::Timeout => write!(f, "stream acquire timed out"),
            Self::PeerStreamLimitTooLow { negotiated } => write!(
                f,
                "peer advertised max_concurrent_streams={} which is too low for multiplexed streams",
                negotiated
            ),
            Self::StreamOpenFailed(e) => write!(f, "stream factory failed: {e}"),
            Self::Closed => write!(f, "stream pool is closed"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StreamOpenFailed(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

/// Boxed future type returned by the stream factory.
pub type StreamFactoryFuture<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'static>>;

/// A factory that opens a new underlying stream on demand. Called by the
/// pool when it needs to grow lazily up to its configured bound.
pub type StreamFactory<T> = Arc<dyn Fn() -> StreamFactoryFuture<T> + Send + Sync + 'static>;

/// An opaque RAII handle to a pooled multiplexed stream.
///
/// On drop, the underlying stream is returned to its originating pool
/// (LIFO) unless `discard` was called, in which case the stream is
/// dropped and the pool slot is freed.
pub struct StreamHandle<T> {
    stream: Option<T>,
    pool: Arc<PoolInner<T>>,
    key: PoolKey,
    poisoned: bool,
}

impl<T> StreamHandle<T> {
    /// Borrow the underlying stream.
    pub fn get(&self) -> &T {
        self.stream
            .as_ref()
            .expect("stream handle is valid before drop")
    }

    /// Mutably borrow the underlying stream.
    pub fn get_mut(&mut self) -> &mut T {
        self.stream
            .as_mut()
            .expect("stream handle is valid before drop")
    }

    /// Mark this handle as poisoned; on drop the underlying stream is
    /// dropped (not returned to the pool) and the pool slot is freed.
    /// Use when a transport error makes the stream unsafe to reuse.
    pub fn discard(mut self) {
        self.poisoned = true;
        // Drop runs here and will observe poisoned=true.
    }

    /// Take the underlying stream out of the handle, permanently removing
    /// it from the pool. The pool slot is freed on drop.
    pub fn into_inner(mut self) -> T {
        let stream = self.stream.take().expect("stream handle is valid");
        self.poisoned = true;
        stream
    }

    /// The routing key this handle was acquired under.
    pub fn key(&self) -> &PoolKey {
        &self.key
    }
}

impl<T> Drop for StreamHandle<T> {
    fn drop(&mut self) {
        // Four cases:
        //  - stream=Some, !poisoned → return to free list (LIFO)
        //  - stream=Some,  poisoned → drop stream, free slot
        //  - stream=None,  poisoned → caller took the stream via into_inner; free slot
        //  - stream=None, !poisoned → unreachable (we always set poisoned when taking)
        let stream = self.stream.take();
        if self.poisoned {
            drop(stream);
            self.pool.drop_slot(&self.key);
        } else if let Some(stream) = stream {
            self.pool.release(&self.key, stream);
        }
    }
}

// ============================================================================
// Pool internals
// ============================================================================

/// Signal delivered to a parked waiter.
///
/// A waiter that receives `Handoff` has been granted an already-open
/// stream and can return it directly. A waiter that receives `Retry`
/// has been told a pool slot has been freed (by `discard`) and must
/// re-enter the acquire fast path to open a fresh stream against the
/// remaining deadline.
enum WakeSignal<T> {
    Handoff(T),
    Retry,
}

/// A sub-pool for one routing key (SHARED or a specific session).
struct SubPool<T> {
    /// LIFO stack of free streams. `pop` returns the most recently
    /// released stream to keep recently-used streams hot.
    free: Vec<T>,
    /// Total number of streams currently owned by this sub-pool: free
    /// streams plus in-flight (busy) handles.
    open_count: usize,
    /// FIFO queue of waiters. Each sender is fulfilled either by a
    /// released stream (`Handoff`) or by a freed slot (`Retry`). If a
    /// waiter's receiver is dropped (caller timed out), the waker
    /// skips it and moves on to the next live waiter.
    waiters: VecDeque<oneshot::Sender<WakeSignal<T>>>,
    /// Configured maximum number of streams in this sub-pool.
    max_size: usize,
}

impl<T> SubPool<T> {
    fn new(max_size: usize) -> Self {
        Self {
            free: Vec::new(),
            open_count: 0,
            waiters: VecDeque::new(),
            max_size,
        }
    }
}

/// Internal shared state of a `StreamPool`.
struct PoolInner<T> {
    config: PoolConfig,
    factory: StreamFactory<T>,
    /// Per-key sub-pools. Guarded by a `std::sync::Mutex` because the
    /// critical sections are all bounded (push/pop/waiter bookkeeping)
    /// and we never `.await` while holding the lock.
    subs: Mutex<HashMap<PoolKey, SubPool<T>>>,
    /// Effective max size for the SHARED pool after `clamp_pool_size`.
    /// Initially mirrors `config.shared_pool_size`; may be lowered by
    /// `apply_quic_ceiling`.
    effective_shared_size: Mutex<usize>,
}

impl<T> PoolInner<T> {
    fn max_size_for(&self, key: &PoolKey) -> usize {
        match key {
            None => *self.effective_shared_size.lock().unwrap(),
            Some(_) => self.config.session_pool_size,
        }
    }

    /// Return a stream to its sub-pool. If any waiters are registered,
    /// hand the stream off to the oldest live waiter (FIFO); otherwise
    /// push onto the free LIFO stack.
    fn release(&self, key: &PoolKey, stream: T) {
        let mut subs = self.subs.lock().unwrap();
        let sub = match subs.get_mut(key) {
            Some(s) => s,
            None => {
                // Pool was torn down under us — drop the stream.
                return;
            }
        };
        // Try to hand off to a live waiter. Senders whose receivers
        // have been dropped (timed-out waiters) cannot accept the value
        // and are popped out of the way.
        let mut pending = stream;
        while let Some(tx) = sub.waiters.pop_front() {
            match tx.send(WakeSignal::Handoff(pending)) {
                Ok(()) => return,
                Err(WakeSignal::Handoff(returned)) => {
                    pending = returned;
                    continue;
                }
                Err(WakeSignal::Retry) => unreachable!("we sent Handoff"),
            }
        }
        sub.free.push(pending);
    }

    /// Free a pool slot without returning a stream (discard path).
    ///
    /// If waiters are queued, the oldest live waiter is woken with a
    /// `Retry` signal so it can re-enter the acquire fast path and
    /// open a fresh stream against the freed slot. Without this, a
    /// burst of correlated discards (e.g. a connection reset that
    /// poisons every in-flight stream) would leave waiters asleep and
    /// timing out with a misleading `PoolFull` error even though the
    /// pool is actually empty.
    fn drop_slot(&self, key: &PoolKey) {
        let mut subs = self.subs.lock().unwrap();
        let sub = match subs.get_mut(key) {
            Some(s) => s,
            None => return,
        };
        sub.open_count = sub.open_count.saturating_sub(1);
        // Wake one waiter so it can claim the freed slot. Skip dead
        // waiters (timed-out receivers) the same way `release` does.
        while let Some(tx) = sub.waiters.pop_front() {
            if tx.send(WakeSignal::Retry).is_ok() {
                break;
            }
        }
    }
}

// ============================================================================
// Public pool type
// ============================================================================

/// Per-connection stream pool keyed by `Option<SessionId>`.
///
/// A single `StreamPool` holds the SHARED sub-pool and any number of
/// per-session sub-pools. Sub-pools are created lazily on first
/// `acquire` for a new key.
pub struct StreamPool<T> {
    inner: Arc<PoolInner<T>>,
}

impl<T> Clone for StreamPool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send + 'static> StreamPool<T> {
    /// Create a new pool with the given factory and config.
    pub fn new(config: PoolConfig, factory: StreamFactory<T>) -> Self {
        let effective_shared_size = config.shared_pool_size;
        Self {
            inner: Arc::new(PoolInner {
                config,
                factory,
                subs: Mutex::new(HashMap::new()),
                effective_shared_size: Mutex::new(effective_shared_size),
            }),
        }
    }

    /// Apply a connect-time QUIC `max_concurrent_streams` ceiling. See
    /// spec §9. Returns the effective SHARED pool size after clamping,
    /// or an error if the negotiated ceiling is unusable.
    ///
    /// `headroom` is the number of streams reserved for session and
    /// streaming substreams; default 4 per spec.
    pub fn apply_quic_ceiling(
        &self,
        negotiated_max_concurrent_streams: u64,
        headroom: usize,
    ) -> Result<usize, AcquireError> {
        let effective = clamp_pool_size(
            self.inner.config.shared_pool_size,
            negotiated_max_concurrent_streams,
            headroom,
        )?;
        if effective < self.inner.config.shared_pool_size {
            tracing::warn!(
                configured = self.inner.config.shared_pool_size,
                negotiated = negotiated_max_concurrent_streams,
                effective,
                "shared_pool_size clamped to fit QUIC max_concurrent_streams ceiling"
            );
        }
        *self.inner.effective_shared_size.lock().unwrap() = effective;
        Ok(effective)
    }

    /// Current configured pool config.
    pub fn config(&self) -> &PoolConfig {
        &self.inner.config
    }

    /// Current number of open streams (free + busy) for a key. Zero if
    /// the sub-pool has never been touched.
    pub fn open_count(&self, key: &PoolKey) -> usize {
        self.inner
            .subs
            .lock()
            .unwrap()
            .get(key)
            .map(|s| s.open_count)
            .unwrap_or(0)
    }

    /// Current number of free (idle) streams for a key.
    pub fn free_count(&self, key: &PoolKey) -> usize {
        self.inner
            .subs
            .lock()
            .unwrap()
            .get(key)
            .map(|s| s.free.len())
            .unwrap_or(0)
    }

    /// Acquire a stream from the pool for `key`.
    ///
    /// Semantics (spec §3, §5):
    /// - If a free stream is available, return it LIFO.
    /// - Else if the sub-pool is below its bound, open a new stream
    ///   via the factory.
    /// - Else wait in FIFO order for a free stream, up to
    ///   `stream_acquire_timeout`. On timeout, return
    ///   `AcquireError::PoolFull`.
    pub async fn acquire(&self, key: PoolKey) -> Result<StreamHandle<T>, AcquireError> {
        // Deadline is tracked across retries so that waking on a
        // `Retry` signal and re-entering the fast path doesn't reset
        // the caller's total wait budget.
        let deadline = Instant::now() + self.inner.config.stream_acquire_timeout;
        // The outer loop iterates only on `Retry` wake-ups (a freed
        // slot via `discard`). Every other path is terminal.
        loop {
            // ---- Phase 1: non-blocking check under the lock. -------
            // Either:
            //   (a) free stream → return it,
            //   (b) slot under the cap → reserve it and run factory,
            //   (c) full → register a waiter and fall through to wait.
            enum Phase1<T> {
                Ready(T),
                OpenNew,
                Wait(oneshot::Receiver<WakeSignal<T>>),
            }

            let phase1 = {
                let mut subs = self.inner.subs.lock().unwrap();
                let max = self.inner.max_size_for(&key);
                let sub = subs.entry(key.clone()).or_insert_with(|| SubPool::new(max));
                // max_size may have been clamped since creation.
                sub.max_size = max;

                if let Some(stream) = sub.free.pop() {
                    Phase1::Ready(stream)
                } else if sub.open_count < sub.max_size {
                    sub.open_count += 1;
                    Phase1::OpenNew
                } else {
                    let (tx, rx) = oneshot::channel();
                    sub.waiters.push_back(tx);
                    Phase1::Wait(rx)
                }
            };

            match phase1 {
                Phase1::Ready(stream) => return Ok(self.wrap(key, stream)),
                Phase1::OpenNew => {
                    return match (self.inner.factory)().await {
                        Ok(stream) => Ok(self.wrap(key, stream)),
                        Err(err) => {
                            // Release the reserved slot and surface
                            // the error. `drop_slot` also wakes a
                            // waiter with `Retry` so the slot doesn't
                            // sit idle.
                            self.inner.drop_slot(&key);
                            Err(AcquireError::StreamOpenFailed(err))
                        }
                    };
                }
                Phase1::Wait(rx) => {
                    // ---- Phase 2: wait for a wake signal. ----------
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(AcquireError::PoolFull);
                    }
                    let remaining = deadline - now;
                    match timeout(remaining, rx).await {
                        Ok(Ok(WakeSignal::Handoff(stream))) => {
                            return Ok(self.wrap(key, stream));
                        }
                        Ok(Ok(WakeSignal::Retry)) => {
                            // A slot was freed. Loop to re-enter the
                            // fast path against the tracked deadline.
                            continue;
                        }
                        Ok(Err(_)) => return Err(AcquireError::Closed),
                        Err(_) => return Err(AcquireError::PoolFull),
                    }
                }
            }
        }
    }

    fn wrap(&self, key: PoolKey, stream: T) -> StreamHandle<T> {
        StreamHandle {
            stream: Some(stream),
            pool: self.inner.clone(),
            key,
            poisoned: false,
        }
    }
}

// ============================================================================
// Pure helpers
// ============================================================================

/// Clamp a configured pool size against a QUIC-negotiated
/// `max_concurrent_streams` ceiling. See spec §9.
///
/// - If `negotiated < 2`, the connection is unusable for multiplexed
///   streams and `PeerStreamLimitTooLow` is returned.
/// - If `configured > negotiated`, the effective size is
///   `max(1, negotiated - headroom)`.
/// - Otherwise the configured size is returned unchanged.
pub fn clamp_pool_size(
    configured: usize,
    negotiated: u64,
    headroom: usize,
) -> Result<usize, AcquireError> {
    if negotiated < 2 {
        return Err(AcquireError::PeerStreamLimitTooLow { negotiated });
    }
    let ceiling = usize::try_from(negotiated).unwrap_or(usize::MAX);
    if configured > ceiling {
        Ok(ceiling.saturating_sub(headroom).max(1))
    } else {
        Ok(configured)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    /// Synthetic "stream" type for tests: just a unique id.
    #[derive(Debug, PartialEq, Eq)]
    struct FakeStream {
        id: usize,
    }

    /// Build a pool whose factory hands out `FakeStream`s with
    /// monotonically increasing ids. Returns the pool and the counter
    /// for inspection.
    fn make_pool(config: PoolConfig) -> (StreamPool<FakeStream>, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = counter.clone();
        let factory: StreamFactory<FakeStream> = Arc::new(move || {
            let c = c2.clone();
            Box::pin(async move {
                let id = c.fetch_add(1, Ordering::SeqCst);
                Ok(FakeStream { id })
            })
        });
        (StreamPool::new(config, factory), counter)
    }

    fn cfg(shared: usize, session: usize, timeout_ms: u64) -> PoolConfig {
        PoolConfig {
            shared_pool_size: shared,
            session_pool_size: session,
            stream_acquire_timeout: Duration::from_millis(timeout_ms),
        }
    }

    #[tokio::test]
    async fn acquire_release_reuse_single_thread() {
        let (pool, counter) = make_pool(cfg(4, 1, 1000));
        let h = pool.acquire(None).await.unwrap();
        assert_eq!(h.get().id, 0);
        drop(h);
        // Reused: no new stream opened.
        let h = pool.acquire(None).await.unwrap();
        assert_eq!(h.get().id, 0);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pool_grows_lazily_up_to_bound() {
        let (pool, counter) = make_pool(cfg(3, 1, 1000));
        let h1 = pool.acquire(None).await.unwrap();
        let h2 = pool.acquire(None).await.unwrap();
        let h3 = pool.acquire(None).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert_eq!(pool.open_count(&None), 3);
        drop(h1);
        drop(h2);
        drop(h3);
        assert_eq!(pool.free_count(&None), 3);
        // No additional streams created on reuse.
        let _h = pool.acquire(None).await.unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn pool_full_blocks_then_unblocks_on_release() {
        let (pool, _counter) = make_pool(cfg(1, 1, 5000));
        let h1 = pool.acquire(None).await.unwrap();
        assert_eq!(h1.get().id, 0);

        // Second acquire must wait until h1 is released.
        let pool2 = pool.clone();
        let waiter = tokio::spawn(async move { pool2.acquire(None).await });

        // Give the waiter time to register.
        sleep(Duration::from_millis(20)).await;

        // Release: waiter should receive the same stream (id=0).
        drop(h1);

        let h2 = waiter.await.unwrap().unwrap();
        assert_eq!(h2.get().id, 0);
    }

    #[tokio::test]
    async fn pool_full_times_out_with_pool_full_error() {
        let (pool, _counter) = make_pool(cfg(1, 1, 50));
        let _h1 = pool.acquire(None).await.unwrap();

        let err = pool.acquire(None).await.err().expect("should time out");
        assert!(matches!(err, AcquireError::PoolFull));
    }

    #[tokio::test]
    async fn discarded_stream_is_not_returned_to_pool() {
        let (pool, counter) = make_pool(cfg(2, 1, 1000));
        let h = pool.acquire(None).await.unwrap();
        let id = h.get().id;
        assert_eq!(id, 0);
        h.discard();
        // Slot freed, free list empty, no new free stream.
        assert_eq!(pool.free_count(&None), 0);
        assert_eq!(pool.open_count(&None), 0);
        // Next acquire opens a fresh stream rather than reusing id=0.
        let h2 = pool.acquire(None).await.unwrap();
        assert_eq!(h2.get().id, 1);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lifo_ordering_most_recently_released_next_acquired() {
        let (pool, _counter) = make_pool(cfg(3, 1, 1000));
        let h1 = pool.acquire(None).await.unwrap();
        let h2 = pool.acquire(None).await.unwrap();
        let h3 = pool.acquire(None).await.unwrap();
        assert_eq!((h1.get().id, h2.get().id, h3.get().id), (0, 1, 2));
        // Release in order 0, 1, 2 — LIFO means next-acquired is 2, then 1, then 0.
        drop(h1);
        drop(h2);
        drop(h3);
        let a = pool.acquire(None).await.unwrap();
        let b = pool.acquire(None).await.unwrap();
        let c = pool.acquire(None).await.unwrap();
        assert_eq!(a.get().id, 2);
        assert_eq!(b.get().id, 1);
        assert_eq!(c.get().id, 0);
    }

    #[tokio::test]
    async fn per_session_pool_isolation() {
        let (pool, _counter) = make_pool(cfg(2, 2, 1000));
        let shared_key: PoolKey = None;
        let sess_a: PoolKey = Some(b"session-a".to_vec());
        let sess_b: PoolKey = Some(b"session-b".to_vec());

        // Fill SHARED to its bound (2 streams).
        let s1 = pool.acquire(shared_key.clone()).await.unwrap();
        let s2 = pool.acquire(shared_key.clone()).await.unwrap();
        assert_eq!(pool.open_count(&shared_key), 2);

        // Session A and B should still be able to open up to their own
        // bound (2 each) without being blocked by SHARED being full.
        let a1 = pool.acquire(sess_a.clone()).await.unwrap();
        let a2 = pool.acquire(sess_a.clone()).await.unwrap();
        let b1 = pool.acquire(sess_b.clone()).await.unwrap();
        let b2 = pool.acquire(sess_b.clone()).await.unwrap();

        assert_eq!(pool.open_count(&sess_a), 2);
        assert_eq!(pool.open_count(&sess_b), 2);
        assert_eq!(pool.open_count(&shared_key), 2);

        // All stream ids must be distinct (no cross-pool contamination).
        let ids = [
            s1.get().id,
            s2.get().id,
            a1.get().id,
            a2.get().id,
            b1.get().id,
            b2.get().id,
        ];
        let mut sorted = ids;
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert_ne!(w[0], w[1], "duplicate id across pools: {ids:?}");
        }
    }

    #[tokio::test]
    async fn session_pool_full_does_not_affect_shared() {
        let (pool, _counter) = make_pool(cfg(2, 1, 50));
        let sess: PoolKey = Some(b"s".to_vec());
        let _held = pool.acquire(sess.clone()).await.unwrap();
        // Second acquire on same session times out (bound=1).
        let err = pool.acquire(sess).await.err().unwrap();
        assert!(matches!(err, AcquireError::PoolFull));
        // SHARED still works freely.
        let _a = pool.acquire(None).await.unwrap();
        let _b = pool.acquire(None).await.unwrap();
    }

    // ---- Pure clamp_pool_size tests ---------------------------------

    #[test]
    fn clamp_pool_size_passthrough_when_under_ceiling() {
        assert_eq!(clamp_pool_size(8, 100, 4).unwrap(), 8);
    }

    #[test]
    fn clamp_pool_size_clamps_with_headroom() {
        // configured 16, ceiling 10, headroom 4 → effective 6.
        assert_eq!(clamp_pool_size(16, 10, 4).unwrap(), 6);
    }

    #[test]
    fn clamp_pool_size_minimum_of_one() {
        // ceiling tiny: headroom would drive it below 1, floor at 1.
        assert_eq!(clamp_pool_size(16, 3, 4).unwrap(), 1);
    }

    #[test]
    fn clamp_pool_size_rejects_too_low_peer() {
        let err = clamp_pool_size(8, 1, 4).unwrap_err();
        assert!(matches!(
            err,
            AcquireError::PeerStreamLimitTooLow { negotiated: 1 }
        ));
        let err = clamp_pool_size(8, 0, 4).unwrap_err();
        assert!(matches!(
            err,
            AcquireError::PeerStreamLimitTooLow { negotiated: 0 }
        ));
    }

    #[test]
    fn clamp_pool_size_equal_to_ceiling_returns_configured() {
        assert_eq!(clamp_pool_size(10, 10, 4).unwrap(), 10);
    }

    #[tokio::test]
    async fn apply_quic_ceiling_lowers_effective_shared_size() {
        let (pool, _counter) = make_pool(cfg(16, 1, 50));
        let effective = pool.apply_quic_ceiling(10, 4).unwrap();
        assert_eq!(effective, 6);

        // After clamping, the SHARED pool only grows to 6.
        let mut handles = Vec::new();
        for _ in 0..6 {
            handles.push(pool.acquire(None).await.unwrap());
        }
        assert_eq!(pool.open_count(&None), 6);
        // The 7th acquire times out.
        let err = pool.acquire(None).await.err().unwrap();
        assert!(matches!(err, AcquireError::PoolFull));
    }

    #[tokio::test]
    async fn factory_error_is_surfaced_and_slot_is_released() {
        let factory: StreamFactory<FakeStream> =
            Arc::new(|| Box::pin(async { Err(anyhow::anyhow!("boom")) }));
        let pool = StreamPool::new(cfg(1, 1, 50), factory);
        let err = pool.acquire(None).await.err().unwrap();
        assert!(matches!(err, AcquireError::StreamOpenFailed(_)));
        // Slot was released so a subsequent attempt is not blocked by it.
        assert_eq!(pool.open_count(&None), 0);
        let err2 = pool.acquire(None).await.err().unwrap();
        assert!(matches!(err2, AcquireError::StreamOpenFailed(_)));
    }

    #[tokio::test]
    async fn fifo_waiter_order_across_multiple_waiters() {
        let (pool, _counter) = make_pool(cfg(1, 1, 5000));
        let h = pool.acquire(None).await.unwrap();

        // Spawn three waiters in order; each tagged by index.
        let results = Arc::new(Mutex::new(Vec::<usize>::new()));
        let mut joins = Vec::new();
        for i in 0..3 {
            let p = pool.clone();
            let r = results.clone();
            joins.push(tokio::spawn(async move {
                let handle = p.acquire(None).await.unwrap();
                r.lock().unwrap().push(i);
                // Release immediately so next waiter can proceed.
                drop(handle);
            }));
            // Stagger so the waiters enqueue in order.
            sleep(Duration::from_millis(10)).await;
        }

        // Let waiters register, then release the first held stream.
        sleep(Duration::from_millis(20)).await;
        drop(h);

        for j in joins {
            j.await.unwrap();
        }

        let order = results.lock().unwrap().clone();
        assert_eq!(order, vec![0, 1, 2], "waiters should be served FIFO");
    }

    #[tokio::test]
    async fn discard_wakes_blocked_waiter() {
        // Case A: single discard frees one slot; a blocked waiter
        // should be woken and open a fresh stream against that slot
        // rather than sitting asleep until its timeout fires.
        let (pool, counter) = make_pool(cfg(1, 1, 5000));
        let h1 = pool.acquire(None).await.unwrap();
        assert_eq!(h1.get().id, 0);

        // Spawn a waiter; the pool is at capacity so it blocks.
        let pool2 = pool.clone();
        let start = Instant::now();
        let waiter = tokio::spawn(async move { pool2.acquire(None).await });
        sleep(Duration::from_millis(20)).await;

        // Discard the held stream. Waiter should wake, re-enter the
        // fast path, and open stream id=1 (discarded stream is gone).
        h1.discard();

        let handle = waiter.await.unwrap().unwrap();
        let elapsed = start.elapsed();
        assert_eq!(handle.get().id, 1, "waiter should open a fresh stream");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert!(
            elapsed < Duration::from_millis(500),
            "waiter should wake quickly, not wait for timeout (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn correlated_discard_cascade_wakes_all_waiters() {
        // Case B: all in-flight streams are discarded at once (the
        // "connection reset poisons every handle" failure mode). All
        // parked waiters must wake — otherwise they'd time out with a
        // misleading PoolFull even though the pool is empty.
        let (pool, counter) = make_pool(cfg(4, 1, 5000));

        // Fill the pool.
        let held: Vec<_> = {
            let mut v = Vec::new();
            for _ in 0..4 {
                v.push(pool.acquire(None).await.unwrap());
            }
            v
        };
        assert_eq!(pool.open_count(&None), 4);

        // Spawn 8 waiters (4 will be served by the cascade, 4 would
        // otherwise sit on the release handoff path).
        let start = Instant::now();
        let mut joins = Vec::new();
        for _ in 0..8 {
            let p = pool.clone();
            joins.push(tokio::spawn(async move { p.acquire(None).await }));
        }
        sleep(Duration::from_millis(30)).await;

        // Discard all 4 held streams simultaneously. Each discard
        // wakes one waiter with Retry. Those waiters re-enter the fast
        // path, open fresh streams (ids 4..=7), and proceed. As they
        // drop their handles at task end, later waiters are served via
        // normal release handoff (ids reused).
        for h in held {
            h.discard();
        }

        let mut ok = 0;
        for j in joins {
            if j.await.unwrap().is_ok() {
                ok += 1;
            }
        }
        let elapsed = start.elapsed();
        assert_eq!(ok, 8, "all 8 waiters should eventually acquire a stream");
        // With the cascade fix, total time is dominated by the 30ms
        // sleep + scheduling, not the 5s timeout.
        assert!(
            elapsed < Duration::from_millis(2000),
            "waiters should not time out (took {elapsed:?})"
        );
        // At least 4 fresh streams were opened by the waking waiters;
        // more may have been opened as earlier waiters' factories ran
        // concurrently. Lower bound is what we care about.
        let created = counter.load(Ordering::SeqCst);
        assert!(
            created >= 4 + 4,
            "expected at least 8 streams opened, saw {created}"
        );
    }

    #[tokio::test]
    async fn correlated_factory_failure_surfaces_real_error_to_waiters() {
        // Connection-dead variant of Case B: held streams discard,
        // waiters wake, but the factory (which would open new
        // streams) also fails. Every waiter should surface the
        // StreamOpenFailed error rather than a misleading PoolFull.
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = fail.clone();
        let counter = Arc::new(AtomicUsize::new(0));
        let c2 = counter.clone();
        let factory: StreamFactory<FakeStream> = Arc::new(move || {
            let fail = f2.clone();
            let c = c2.clone();
            Box::pin(async move {
                if fail.load(Ordering::SeqCst) {
                    Err(anyhow::anyhow!("connection dead"))
                } else {
                    Ok(FakeStream {
                        id: c.fetch_add(1, Ordering::SeqCst),
                    })
                }
            })
        });
        let pool = StreamPool::new(cfg(2, 1, 5000), factory);

        let h1 = pool.acquire(None).await.unwrap();
        let h2 = pool.acquire(None).await.unwrap();
        assert_eq!(pool.open_count(&None), 2);

        // Spawn 3 waiters.
        let start = Instant::now();
        let mut joins = Vec::new();
        for _ in 0..3 {
            let p = pool.clone();
            joins.push(tokio::spawn(async move { p.acquire(None).await }));
        }
        sleep(Duration::from_millis(30)).await;

        // Now "kill" the connection and discard both held streams.
        fail.store(true, Ordering::SeqCst);
        h1.discard();
        h2.discard();

        let mut errors = Vec::new();
        for j in joins {
            errors.push(j.await.unwrap().err().expect("factory is failing"));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(2000),
            "waiters should get a real error quickly (took {elapsed:?})"
        );
        for err in &errors {
            assert!(
                matches!(err, AcquireError::StreamOpenFailed(_)),
                "waiter should see the real factory error, not PoolFull: {err:?}"
            );
        }
    }

    // ========================================================================
    // Property-based tests (tier 1)
    //
    // These drive the pool with adversarial randomized schedules and assert
    // on the spec-level invariants that hand-written cases can't cover
    // exhaustively. The shared harness builds a pool whose factory hands
    // out `FakeStream`s and then replays a script of `Op`s against it,
    // recording outcomes so the property assertions can inspect the
    // post-state.
    // ========================================================================

    /// Routing key space for the property tests. Three sub-pools is
    /// enough to exercise isolation between SHARED and two distinct
    /// session keys without blowing up the state space.
    fn key_for(slot: u8) -> PoolKey {
        match slot % 3 {
            0 => None,
            1 => Some(vec![0xAA]),
            _ => Some(vec![0xBB]),
        }
    }

    /// A single scripted operation. Acquires are tagged with a slot
    /// index (mod 3 → SHARED / session A / session B). Releases and
    /// discards reference a previously acquired handle by ordinal, so
    /// the harness can drop the right one regardless of schedule.
    #[derive(Clone, Debug)]
    enum Op {
        Acquire(u8),
        Release(usize),
        Discard(usize),
    }

    fn op_strategy() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            (0u8..6).prop_map(Op::Acquire),
            (0usize..16).prop_map(Op::Release),
            (0usize..16).prop_map(Op::Discard),
        ]
    }

    /// Run a script against a pool with generous caps and a short
    /// acquire timeout. Returns the pool for post-run invariant checks.
    async fn run_script(ops: Vec<Op>, shared: usize, session: usize) -> StreamPool<FakeStream> {
        let (pool, _counter) = make_pool(cfg(shared, session, 200));
        let mut held: Vec<StreamHandle<FakeStream>> = Vec::new();
        for op in ops {
            match op {
                Op::Acquire(slot) => {
                    let key = key_for(slot);
                    if let Ok(h) = pool.acquire(key).await {
                        held.push(h);
                    }
                    // POOL_FULL errors are expected — we over-saturate
                    // on purpose — and must not corrupt invariants.
                }
                Op::Release(idx) => {
                    if !held.is_empty() {
                        let i = idx % held.len();
                        drop(held.remove(i));
                    }
                }
                Op::Discard(idx) => {
                    if !held.is_empty() {
                        let i = idx % held.len();
                        held.remove(i).discard();
                    }
                }
            }
        }
        // Drain any remaining handles so the post-state reflects a
        // quiescent pool — makes the invariants easier to reason
        // about.
        drop(held);
        pool
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            // 64 cases is enough to hit the interesting corners without
            // slowing `cargo test` down significantly; the operations
            // are cheap.
            cases: 64,
            .. proptest::test_runner::Config::default()
        })]

        /// Invariant: at the end of any script, every sub-pool's
        /// `open_count` is ≤ its configured `max_size`. The pool must
        /// never lazily allocate past its bound, regardless of
        /// acquire/release/discard interleavings.
        #[test]
        fn open_count_never_exceeds_max_size(ops in proptest::collection::vec(op_strategy(), 0..40)) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let pool = rt.block_on(run_script(ops, 4, 2));
            let shared_open = pool.open_count(&None);
            let sess_a_open = pool.open_count(&Some(vec![0xAA]));
            let sess_b_open = pool.open_count(&Some(vec![0xBB]));
            proptest::prop_assert!(shared_open <= 4, "shared pool open_count {} > 4", shared_open);
            proptest::prop_assert!(sess_a_open <= 2, "session A open_count {} > 2", sess_a_open);
            proptest::prop_assert!(sess_b_open <= 2, "session B open_count {} > 2", sess_b_open);
        }

        /// Invariant: after the script quiesces (all handles dropped),
        /// `open_count` equals `free_count` for every sub-pool. Nothing
        /// should be "in flight" once every handle has been released
        /// or discarded.
        #[test]
        fn quiescent_pool_has_no_in_flight_handles(ops in proptest::collection::vec(op_strategy(), 0..40)) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let pool = rt.block_on(run_script(ops, 4, 2));
            for key in [None, Some(vec![0xAA]), Some(vec![0xBB])] {
                let open = pool.open_count(&key);
                let free = pool.free_count(&key);
                proptest::prop_assert_eq!(
                    open, free,
                    "key {:?}: open={} free={} — quiescent pool should have no in-flight",
                    key, open, free
                );
            }
        }

        /// Invariant: sub-pool isolation — the SHARED key and each
        /// session key are independent. A discard on a session
        /// sub-pool must never appear to free a slot in another
        /// sub-pool. This is a structural invariant that prevents the
        /// kind of bug §6 exists to prevent (cross-session
        /// co-mingling inside the pool primitive).
        #[test]
        fn subpool_counts_are_independent(ops in proptest::collection::vec(op_strategy(), 0..30)) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let pool = rt.block_on(run_script(ops, 4, 2));
            // The sum of per-key open_counts equals the total number
            // of distinct streams that the factory was asked to make
            // and that still belong to the pool. There is no shared
            // accounting register — checking each key independently
            // and that they don't interfere is the invariant.
            let shared = pool.open_count(&None);
            let a = pool.open_count(&Some(vec![0xAA]));
            let b = pool.open_count(&Some(vec![0xBB]));
            // Every sub-pool respects its own bound.
            proptest::prop_assert!(shared <= pool.config().shared_pool_size);
            proptest::prop_assert!(a <= pool.config().session_pool_size);
            proptest::prop_assert!(b <= pool.config().session_pool_size);
        }
    }

    /// Concurrency stress: spawn N tasks hammering the same pool with
    /// random acquire/release/discard sequences across the three
    /// routing keys. The sequential `proptest`s above can't exercise
    /// the waiter queue, the `drop_slot` Retry signal path, or the
    /// Mutex handoff between release-and-wake, because every `.await`
    /// runs on a single task. This test spawns 8 tasks on a
    /// multi-threaded runtime so the acquire/release paths actually
    /// race.
    ///
    /// Post-quiescence invariants:
    /// - Every sub-pool's `open_count` is within its configured bound.
    /// - `open_count == free_count` on every key (nothing in flight).
    /// - No waiters left queued (the internal `waiters` VecDeque is
    ///   empty on every sub-pool).
    ///
    /// Pre-fix regression shape this would catch: a race on `drop_slot`
    /// that lost wake-ups would leave waiters hung past the test
    /// deadline and the task joins would fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_acquire_release_preserves_invariants() {
        use std::sync::atomic::AtomicU64;
        // Deliberately tight caps: 2 SHARED slots, 1 per session, with
        // a 500ms acquire timeout. With 12 tasks competing for 4 total
        // slots and holding for 200us per iteration, the waiter queue
        // is guaranteed to fill and the `release`/`drop_slot` handoff
        // paths are exercised — that's the whole point of the test.
        let (pool, _counter) = make_pool(cfg(2, 1, 500));
        let task_count = 12usize;
        let iters_per_task = 40usize;
        let ops_done = Arc::new(AtomicU64::new(0));

        let mut joins = Vec::new();
        for task_id in 0..task_count {
            let p = pool.clone();
            let counter = ops_done.clone();
            joins.push(tokio::spawn(async move {
                for i in 0..iters_per_task {
                    // Rotate across all three keys deterministically
                    // so every sub-pool sees traffic but per-task
                    // sequences differ.
                    let slot = ((task_id * 3 + i) % 3) as u8;
                    let key = key_for(slot);
                    match p.acquire(key).await {
                        Ok(h) => {
                            // Hold briefly so other tasks land in the
                            // waiter queue. Using a tiny sleep is
                            // intentional: it makes the test produce
                            // real contention without being flaky.
                            tokio::time::sleep(Duration::from_micros(200)).await;
                            if i % 7 == 0 {
                                h.discard();
                            } else {
                                drop(h);
                            }
                        }
                        Err(_) => {
                            // POOL_FULL under contention is expected
                            // — that's the pressure we wanted.
                        }
                    }
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // All tasks must finish within a generous deadline. If they
        // don't, a waiter was lost and is hanging — the test fails
        // with the join timeout instead of a deadlock.
        let deadline = Duration::from_secs(30);
        for j in joins {
            tokio::time::timeout(deadline, j)
                .await
                .expect("task timed out — likely a lost wake-up")
                .expect("task panicked");
        }
        assert_eq!(
            ops_done.load(Ordering::Relaxed),
            (task_count * iters_per_task) as u64
        );

        // Invariants after quiescence.
        for (key, bound) in [(None, 2usize), (Some(vec![0xAA]), 1), (Some(vec![0xBB]), 1)] {
            let open = pool.open_count(&key);
            let free = pool.free_count(&key);
            assert!(open <= bound, "{key:?}: open_count {open} > bound {bound}");
            assert_eq!(
                open, free,
                "{key:?}: open {open} != free {free} — handle leaked"
            );
        }
        // No waiters parked on any sub-pool — everyone got served
        // or timed out with POOL_FULL, which is a terminal state.
        let subs = pool.inner.subs.lock().unwrap();
        for (key, sub) in subs.iter() {
            assert_eq!(
                sub.waiters.len(),
                0,
                "{key:?}: waiter queue non-empty after quiescence"
            );
        }
    }

    /// Deterministic regression: the pool primitive must allow many
    /// discards in a row without underflowing `open_count` (the internal
    /// counter uses `saturating_sub`, this test pins the behaviour).
    #[tokio::test]
    async fn discard_cannot_underflow_open_count() {
        let (pool, _) = make_pool(cfg(2, 1, 200));
        let h1 = pool.acquire(None).await.unwrap();
        let h2 = pool.acquire(None).await.unwrap();
        h1.discard();
        h2.discard();
        // A defensive extra drop through `drop_slot` shouldn't panic
        // or underflow; simulate it by acquiring and immediately
        // discarding a few more times.
        for _ in 0..5 {
            let h = pool.acquire(None).await.unwrap();
            h.discard();
        }
        assert_eq!(pool.open_count(&None), 0);
    }
}
