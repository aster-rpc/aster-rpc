//! Ownership of listener tasks and per-call work.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use salvo::conn::{Accepted, Acceptor, ConnCtrl, Coupler, HttpBuilder};
use salvo::Service;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
struct SocketRelease {
    released: AtomicBool,
    notify: tokio::sync::Notify,
}

impl SocketRelease {
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
struct TrackedSocket {
    inner: Option<Arc<dyn quinn::AsyncUdpSocket>>,
    release: Arc<SocketRelease>,
}

impl Drop for TrackedSocket {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.release.released.store(true, Ordering::Release);
        self.release.notify.notify_waiters();
    }
}

#[derive(Debug)]
struct TrackedPoller {
    // Drop the inner poller's socket reference before the wrapper's owner.
    inner: Pin<Box<dyn quinn::UdpPoller>>,
    _owner: Arc<TrackedSocket>,
}

impl quinn::UdpPoller for TrackedPoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.inner.as_mut().poll_writable(cx)
    }
}

impl quinn::AsyncUdpSocket for TrackedSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        let inner = self
            .inner
            .as_ref()
            .expect("live socket")
            .clone()
            .create_io_poller();
        Box::pin(TrackedPoller {
            inner,
            _owner: self,
        })
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit<'_>) -> std::io::Result<()> {
        self.inner.as_ref().expect("live socket").try_send(transmit)
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<std::io::Result<usize>> {
        self.inner
            .as_ref()
            .expect("live socket")
            .poll_recv(cx, bufs, meta)
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.as_ref().expect("live socket").local_addr()
    }
    fn max_transmit_segments(&self) -> usize {
        self.inner
            .as_ref()
            .expect("live socket")
            .max_transmit_segments()
    }
    fn max_receive_segments(&self) -> usize {
        self.inner
            .as_ref()
            .expect("live socket")
            .max_receive_segments()
    }
    fn may_fragment(&self) -> bool {
        self.inner.as_ref().expect("live socket").may_fragment()
    }
}

pub(crate) struct QuicOwner {
    endpoint: Option<quinn::Endpoint>,
    release: Arc<SocketRelease>,
}

impl QuicOwner {
    pub(crate) fn bind(
        config: quinn::ServerConfig,
        addr: SocketAddr,
    ) -> Result<(Self, salvo::conn::quinn::QuinnAcceptor), String> {
        use quinn::Runtime;
        let socket = std::net::UdpSocket::bind(addr).map_err(|e| e.to_string())?;
        let runtime = Arc::new(quinn::TokioRuntime);
        let inner = runtime.wrap_udp_socket(socket).map_err(|e| e.to_string())?;
        let release = Arc::new(SocketRelease::default());
        let socket = Arc::new(TrackedSocket {
            inner: Some(inner),
            release: release.clone(),
        });
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            Default::default(),
            Some(config),
            socket,
            runtime,
        )
        .map_err(|e| e.to_string())?;
        let acceptor = salvo::conn::quinn::QuinnAcceptor::new(
            endpoint.clone(),
            addr,
            CancellationToken::new(),
        );
        Ok((
            Self {
                endpoint: Some(endpoint),
                release,
            },
            acceptor,
        ))
    }

    fn close(&self) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.close(0u32.into(), b"listener stopped");
        }
    }

    async fn shutdown(&mut self) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.close(0u32.into(), b"listener stopped");
            endpoint.wait_idle().await;
        }
        self.endpoint.take();
        // Endpoint's driver owns the final socket reference after wait_idle.
        // Observe the actual socket drop instead of guessing with a rebind loop.
        self.release.wait().await;
    }
}

#[derive(Clone, Default)]
pub(crate) struct ServerControl {
    graceful: CancellationToken,
    force: CancellationToken,
}

struct ConnectionGuard(ConnCtrl);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Salvo's convenience Server detaches accepted connection tasks and returns
/// before force-stopped tasks are joined. Own that lifecycle here while using
/// its existing HTTP/TLS/QUIC couplers and Service without changing protocols.
pub(crate) fn serve_owned<A: Acceptor + 'static>(
    mut acceptor: A,
    service: Service,
) -> (ServerControl, JoinHandle<Result<(), String>>) {
    let control = ServerControl::default();
    let stop = control.clone();
    let task = tokio::spawn(async move {
        let service = Arc::new(service);
        let builder = Arc::new(HttpBuilder::new().auto_alt_svc_header(false));
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                () = stop.force.cancelled() => break,
                () = stop.graceful.cancelled() => break,
                _ = connections.join_next(), if !connections.is_empty() => {},
                accepted = acceptor.accept(None) => {
                    match accepted {
                        Ok(Accepted { coupler, stream, fuse_config, conn_ctrl, local_addr, remote_addr, http_scheme }) => {
                            let handler = service.hyper_handler(local_addr, remote_addr, http_scheme, fuse_config, conn_ctrl.clone(), None);
                            let connection = coupler.couple(stream, handler, builder.clone(), Some(stop.graceful.clone()));
                            let guard = ConnectionGuard(conn_ctrl);
                            connections.spawn(async move {
                                let _guard = guard;
                                let _ = connection.await;
                            });
                        }
                        Err(error) => {
                            eprintln!("aster-transport-salvo: accept failed: {error}");
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
            }
        }
        // No more accepted work can appear after the admission loop exits.
        drop(acceptor);
        while !connections.is_empty() {
            tokio::select! {
                biased;
                () = stop.force.cancelled() => {
                    connections.abort_all();
                    while connections.join_next().await.is_some() {}
                    break;
                }
                _ = connections.join_next() => {},
            }
        }
        Ok(())
    });
    (control, task)
}

#[derive(Clone, Default)]
pub(crate) struct TaskScope {
    inner: Arc<Mutex<Tasks>>,
}

#[derive(Default)]
struct Tasks {
    closed: bool,
    tasks: JoinSet<()>,
}

impl TaskScope {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return false;
        }
        while state.tasks.try_join_next().is_some() {}
        state.tasks.spawn(future);
        true
    }

    pub(crate) fn close_admission(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.tasks.abort_all();
    }

    pub(crate) async fn shutdown(&self) {
        self.close_admission();
        // Leave the set in shared state across suspension: cancelling shutdown
        // must not detach work that a later stop or Drop still has to join.
        while futures_util::future::poll_fn(|cx| {
            self.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tasks
                .poll_join_next(cx)
        })
        .await
        .is_some()
        {}
    }
}

/// A bound HTTPS listener. Both addresses are ready when construction returns.
/// The optional QUIC listener uses the exact TCP address, including an assigned
/// ephemeral port. Call [`Self::stop`] to drain and join its work. Dropping the
/// owner schedules forced cleanup; awaiting stop guarantees ports are released.
pub struct OwnedListener {
    tcp_addr: SocketAddr,
    quic_addr: Option<SocketAddr>,
    handle: ServerControl,
    server: Option<JoinHandle<Result<(), String>>>,
    scope: TaskScope,
    quic: Option<QuicOwner>,
    runtime: tokio::runtime::Handle,
}

impl OwnedListener {
    pub(crate) fn new(
        tcp_addr: SocketAddr,
        quic_addr: Option<SocketAddr>,
        handle: ServerControl,
        server: JoinHandle<Result<(), String>>,
        scope: TaskScope,
        quic: Option<QuicOwner>,
    ) -> Self {
        Self {
            tcp_addr,
            quic_addr,
            handle,
            server: Some(server),
            scope,
            quic,
            runtime: tokio::runtime::Handle::current(),
        }
    }

    pub fn tcp_addr(&self) -> SocketAddr {
        self.tcp_addr
    }

    pub fn quic_addr(&self) -> Option<SocketAddr> {
        self.quic_addr
    }

    /// Stop accepting new calls and allow existing calls to finish within
    /// `grace`. Remaining calls are cancelled after the deadline. Connection
    /// cleanup and QUIC's close/drain finish afterward before ports are released.
    pub async fn stop(&mut self, grace: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now()
            .checked_add(grace)
            .ok_or("listener grace period exceeds the clock range")?;
        self.scope.close_admission();
        let mut result = Ok(());
        if let Some(server) = self.server.as_mut() {
            self.handle.graceful.cancel();
            let joined = match tokio::time::timeout_at(deadline, &mut *server).await {
                Ok(joined) => joined,
                Err(_) => {
                    // Escalate the owned connection loop after the grace.
                    self.scope.cancel();
                    self.handle.force.cancel();
                    if let Some(quic) = &self.quic {
                        quic.close();
                    }
                    server.await
                }
            };
            // Never re-poll a completed JoinHandle after the next suspension.
            self.server.take();
            result = joined.map_err(|e| e.to_string()).and_then(|r| r);
            if result.is_err() {
                self.scope.cancel();
                self.handle.force.cancel();
            }
        }
        if tokio::time::timeout_at(deadline, self.scope.shutdown())
            .await
            .is_err()
        {
            self.scope.cancel();
            self.handle.force.cancel();
            self.scope.shutdown().await;
        }
        if let Some(quic) = &mut self.quic {
            quic.shutdown().await;
        }
        self.quic.take();
        result
    }

    /// Alias for [`Self::stop`].
    pub async fn shutdown(&mut self, grace: Duration) -> Result<(), String> {
        self.stop(grace).await
    }
}

impl Drop for OwnedListener {
    fn drop(&mut self) {
        self.scope.cancel();
        self.handle.force.cancel();
        if let Some(quic) = &self.quic {
            quic.close();
        }
        // A cleanup supervisor retains and joins the owned connection loop.
        let server = self.server.take();
        let scope = self.scope.clone();
        let mut quic = self.quic.take();
        self.runtime.spawn(async move {
            if let Some(server) = server {
                let _ = server.await;
            }
            scope.shutdown().await;
            if let Some(quic) = &mut quic {
                quic.shutdown().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    async fn pending(scope: &TaskScope) -> Arc<AtomicBool> {
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = Dropped(dropped.clone());
        let (started, ready) = tokio::sync::oneshot::channel();
        assert!(scope.spawn(async move {
            let _marker = marker;
            let _ = started.send(());
            std::future::pending::<()>().await;
        }));
        ready.await.unwrap();
        dropped
    }

    #[tokio::test]
    async fn cancelled_scope_shutdown_retains_work_for_a_later_join() {
        let scope = TaskScope::new();
        let dropped = pending(&scope).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(5), scope.shutdown())
                .await
                .is_err()
        );
        assert!(!dropped.load(Ordering::Acquire));
        assert!(!scope.spawn(async {}));
        scope.cancel();
        scope.shutdown().await;
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn completed_server_is_not_repolled_while_scope_waits_for_grace() {
        let scope = TaskScope::new();
        let dropped = pending(&scope).await;
        let server = tokio::spawn(async { Ok(()) });
        let mut listener = OwnedListener::new(
            "127.0.0.1:0".parse().unwrap(),
            None,
            ServerControl::default(),
            server,
            scope,
            None,
        );
        listener.stop(Duration::from_millis(5)).await.unwrap();
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn owner_drop_outside_runtime_context_uses_captured_runtime() {
        let scope = TaskScope::new();
        let dropped = pending(&scope).await;
        let handle = ServerControl::default();
        let force = handle.force.clone();
        let (finished, joined) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            force.cancelled().await;
            let _ = finished.send(());
            Ok(())
        });
        let listener = OwnedListener::new(
            "127.0.0.1:0".parse().unwrap(),
            None,
            handle,
            server,
            scope.clone(),
            None,
        );
        std::thread::spawn(move || drop(listener)).join().unwrap();
        tokio::time::timeout(Duration::from_secs(1), joined)
            .await
            .unwrap()
            .unwrap();
        scope.shutdown().await;
        assert!(dropped.load(Ordering::Acquire));
    }
}
