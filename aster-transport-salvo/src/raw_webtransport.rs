//! Owned raw WebTransport sessions. Bytes are never interpreted as RPC frames.
//! Authentication runs before CONNECT is accepted. An application may use the
//! first bidirectional stream for proof of possession before accepting media.
//! Closing a session closes its underlying QUIC connection, not a WT capsule.

use crate::{listener::TaskScope, OwnedListener, TlsMaterial};
use bytes::Bytes;
use salvo::prelude::*;
use salvo::proto::quic::{BidiStream as _, RecvStream as _, SendStream as _};
use serde::Serialize;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_util::sync::CancellationToken;

type Session = salvo::proto::WebTransportSession<salvo_http3::quinn::Connection, Bytes>;
type Send =
    salvo::proto::webtransport::stream::SendStream<salvo_http3::quinn::SendStream<Bytes>, Bytes>;
type Recv = salvo::proto::webtransport::stream::RecvStream<salvo_http3::quinn::RecvStream, Bytes>;

#[derive(Clone)]
pub struct RawWtConfig {
    pub bind_addr: String,
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub path: String,
    pub max_sessions: usize,
    pub max_streams: usize,
    pub max_datagrams: usize,
    pub max_datagram_bytes: usize,
    pub max_read_bytes: usize,
}
impl RawWtConfig {
    pub fn new(
        bind_addr: impl Into<String>,
        certificate_pem: Vec<u8>,
        private_key_pem: Vec<u8>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            bind_addr: bind_addr.into(),
            certificate_pem,
            private_key_pem,
            path: path.into(),
            max_sessions: 16,
            max_streams: 64,
            max_datagrams: 64,
            max_datagram_bytes: 65536,
            max_read_bytes: 1024 * 1024,
        }
    }
    fn validate(&self) -> Result<(), String> {
        if !self.path.starts_with('/')
            || self.path == "/"
            || self.path.contains(['?', '#', '<', '>', '{', '}', '*', '%'])
            || self.path.starts_with("/aster/")
            || self.path.contains("//")
            || self.path.split('/').any(|s| s == "." || s == "..")
        {
            return Err("raw WebTransport requires an exact non-RPC route".into());
        }
        if [
            self.max_sessions,
            self.max_streams,
            self.max_datagrams,
            self.max_datagram_bytes,
            self.max_read_bytes,
        ]
        .contains(&0)
            || self.max_sessions > 65536
            || self.max_streams > 65536
            || self.max_datagrams > 65536
            || self.max_datagram_bytes > 65536
            || self.max_read_bytes > 16 * 1024 * 1024
        {
            return Err("raw WebTransport limits are zero or excessive".into());
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct RawWtListener {
    inner: Arc<ListenerInner>,
}
struct ListenerInner {
    owner: Mutex<OwnedListener>,
    incoming: Mutex<mpsc::Receiver<RawWtSession>>,
    cancel: CancellationToken,
    addr: SocketAddr,
}
impl RawWtListener {
    pub async fn start_public(config: RawWtConfig) -> Result<Self, String> {
        Self::start(config, None).await
    }
    pub async fn start_authenticated(
        config: RawWtConfig,
        auth: Arc<dyn aster::rpc::Authenticator>,
    ) -> Result<Self, String> {
        Self::start(config, Some(auth)).await
    }
    async fn start(
        config: RawWtConfig,
        auth: Option<Arc<dyn aster::rpc::Authenticator>>,
    ) -> Result<Self, String> {
        config.validate()?;
        let scope = TaskScope::new();
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel(config.max_sessions);
        let handler = RawHandler {
            config: config.clone(),
            auth,
            scope: scope.clone(),
            cancel: cancel.clone(),
            incoming: tx,
            permits: Arc::new(Semaphore::new(config.max_sessions)),
        };
        let app = Router::with_path(config.path.trim_start_matches('/')).goal(handler);
        let owner = crate::tls::start_pem_listener(
            &config.bind_addr,
            TlsMaterial::pem(config.certificate_pem, config.private_key_pem),
            Service::new(app),
            true,
            scope,
        )
        .await?;
        let addr = owner.tcp_addr();
        Ok(Self {
            inner: Arc::new(ListenerInner {
                owner: Mutex::new(owner),
                incoming: Mutex::new(rx),
                cancel,
                addr,
            }),
        })
    }
    pub fn tcp_addr(&self) -> SocketAddr {
        self.inner.addr
    }
    pub fn quic_addr(&self) -> SocketAddr {
        self.inner.addr
    }
    pub async fn accept(&self) -> Result<RawWtSession, String> {
        tokio::select! {biased; _=self.inner.cancel.cancelled()=>Err("listener closed".into()), result=async{self.inner.incoming.lock().await.recv().await}=>result.ok_or_else(||"listener closed".into())}
    }
    /// Cancel admission and raw session operations synchronously; `stop` joins.
    pub fn close(&self) {
        self.inner.cancel.cancel();
    }
    pub async fn stop(&self, grace: Duration) -> Result<(), String> {
        self.close();
        self.inner.owner.lock().await.stop(grace).await
    }
}
impl Drop for ListenerInner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct RawHandler {
    config: RawWtConfig,
    auth: Option<Arc<dyn aster::rpc::Authenticator>>,
    scope: TaskScope,
    cancel: CancellationToken,
    incoming: mpsc::Sender<RawWtSession>,
    permits: Arc<Semaphore>,
}
#[async_trait]
impl Handler for RawHandler {
    async fn handle(&self, req: &mut Request, _: &mut Depot, res: &mut Response, _: &mut FlowCtrl) {
        if req.method() != salvo::http::Method::CONNECT {
            res.status_code(salvo::http::StatusCode::METHOD_NOT_ALLOWED);
            return;
        }
        if req.uri().path() != self.config.path {
            res.status_code(salvo::http::StatusCode::NOT_FOUND);
            return;
        }
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                res.status_code(salvo::http::StatusCode::SERVICE_UNAVAILABLE);
                return;
            }
        };
        if self.cancel.is_cancelled() {
            res.status_code(salvo::http::StatusCode::SERVICE_UNAVAILABLE);
            return;
        }
        let mut identity = aster::rpc::AuthOutcome::default();
        if let Some(auth) = &self.auth {
            let mut metadata: HashMap<String, String> = req
                .headers()
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().into(), v.into())))
                .collect();
            metadata.insert("aster.webtransport.path".into(), self.config.path.clone());
            metadata.insert(
                "aster.webtransport.query".into(),
                req.uri().query().unwrap_or_default().into(),
            );
            let ctx = aster::rpc::AuthContext {
                peer_id: req.remote_addr().to_string(),
                service: "webtransport".into(),
                method: "CONNECT".into(),
                session_id: 0,
                metadata,
            };
            let authenticated = tokio::select! {biased;_=self.cancel.cancelled()=>None,result=auth.authenticate(&ctx)=>result.ok()};
            match authenticated {
                Some(outcome) if outcome.principal.as_ref().is_some_and(|p| !p.is_empty()) => {
                    identity = outcome
                }
                _ => {
                    res.status_code(salvo::http::StatusCode::UNAUTHORIZED);
                    return;
                }
            }
        }

        let Some(connection) = req.quinn_connection() else {
            res.status_code(salvo::http::StatusCode::BAD_REQUEST);
            return;
        };
        let upgraded = tokio::select! {biased;_=self.cancel.cancelled()=>false,result=req.web_transport_mut()=>result.is_ok()};
        if !upgraded {
            connection.close(0u32.into(), b"upgrade cancelled");
            res.status_code(salvo::http::StatusCode::BAD_REQUEST);
            return;
        }
        let Some(session) = req.extensions().get::<Arc<Session>>().cloned() else {
            return;
        };
        let (bi_tx, bi_rx) = mpsc::channel(self.config.max_streams);
        let (uni_tx, uni_rx) = mpsc::channel(self.config.max_streams);
        let (dg_tx, dg_rx) = mpsc::channel(self.config.max_datagrams);
        let transport = Arc::new(SessionTransport {
            session: session.clone(),
            connection: connection.clone(),
        });
        let (closed_tx, _) = tokio::sync::watch::channel(None);
        let inner = Arc::new(SessionInner {
            transport: Arc::downgrade(&transport),
            info: RawWtSessionInfo {
                connection_id: connection.stable_id() as u64,
                session_id: salvo::proto::quic::StreamId::from(session.session_id()).into_inner(),
                peer_address: connection.remote_address().to_string(),
                closed: false,
                principal: identity.principal,
                attributes: identity.attributes,
            },
            final_stats: StdMutex::new(connection.stats()),
            closed: closed_tx,
            local_close: StdMutex::new(None),
            tracked: StdMutex::new(Vec::new()),
            cancel: self.cancel.child_token(),
            bi: Mutex::new(bi_rx),
            uni: Mutex::new(uni_rx),
            datagrams: Mutex::new(dg_rx),
            open_uni: Mutex::new(()),
            open_bi: Mutex::new(()),
            max_read: self.config.max_read_bytes,
            max_datagram: self.config.max_datagram_bytes,
            streams: Arc::new(Semaphore::new(self.config.max_streams)),
            _permit: StdMutex::new(Some(permit)),
        });
        let raw = RawWtSession {
            _owner: Arc::new(SessionOwner(Arc::downgrade(&inner))),
            inner,
        };
        let state = raw.inner.clone();
        let session_done = state.cancel.clone();
        let failed_connection = connection.clone();
        if self.incoming.try_send(raw).is_err() {
            connection.close(0u32.into(), b"session queue full");
            return;
        }
        let cleanup = SessionDriverCleanup {
            state: state.clone(),
            connection: connection.clone(),
        };
        let driver = async move {
            let _cleanup = cleanup;
            let _transport = transport;
            let bidi = async {
                loop {
                    match session.accept_bi().await {
                        Ok(Some(salvo::proto::webtransport::server::AcceptedBi::BidiStream(
                            id,
                            stream,
                        ))) if id == session.session_id() => {
                            let (send, recv) = stream.split();
                            let stream = RawWtStream::new(&state, Some(send), Some(recv)).await?;
                            bi_tx
                                .send(stream)
                                .await
                                .map_err(|_| "session closed".to_string())?;
                        }
                        Ok(Some(_)) => {
                            return Err("unexpected WebTransport session or HTTP request".into())
                        }
                        _ => return Err::<(), String>("connection closed".into()),
                    }
                }
            };
            let uni = async {
                loop {
                    match session.accept_uni().await {
                        Ok(Some((id, recv))) if id == session.session_id() => {
                            let stream = RawWtStream::new(&state, None, Some(recv)).await?;
                            uni_tx
                                .send(stream)
                                .await
                                .map_err(|_| "session closed".to_string())?;
                        }
                        Ok(Some(_)) => {
                            return Err("unexpected WebTransport session or HTTP request".into())
                        }
                        _ => return Err::<(), String>("connection closed".into()),
                    }
                }
            };
            let datagrams = async {
                let mut reader = session.datagram_reader();
                loop {
                    let data = reader.read_datagram().await.map_err(|e| e.to_string())?;
                    if data.stream_id() == salvo::proto::quic::StreamId::from(session.session_id())
                    {
                        let payload = data.into_payload();
                        if payload.len() <= state.max_datagram {
                            match dg_tx.try_send(payload.to_vec()) {
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    return Err::<(), String>("session closed".into())
                                }
                            };
                        }
                    }
                }
            };
            tokio::select! {_ =state.cancel.cancelled()=>{},_=connection.closed()=>{},_=bidi=>{},_=uni=>{},result=datagrams=>{let _:Result<(),String>=result;}}
        };
        if self.scope.spawn(driver) {
            session_done.cancelled().await;
        } else {
            failed_connection.close(0u32.into(), b"listener closed");
        }
    }
}

#[derive(Clone)]
pub struct RawWtSession {
    inner: Arc<SessionInner>,
    _owner: Arc<SessionOwner>,
}
struct SessionOwner(Weak<SessionInner>);
impl Drop for SessionOwner {
    fn drop(&mut self) {
        if let Some(inner) = self.0.upgrade() {
            inner.cancel.cancel();
            if let Some(transport) = inner.transport.upgrade() {
                transport
                    .connection
                    .close(0u32.into(), b"raw session owner dropped");
            }
        }
    }
}
struct SessionInner {
    transport: Weak<SessionTransport>,
    info: RawWtSessionInfo,
    final_stats: StdMutex<quinn::ConnectionStats>,
    closed: tokio::sync::watch::Sender<Option<RawWtCloseInfo>>,
    local_close: StdMutex<Option<RawWtCloseInfo>>,
    tracked: StdMutex<Vec<Weak<StreamInner>>>,
    cancel: CancellationToken,
    bi: Mutex<mpsc::Receiver<RawWtStream>>,
    uni: Mutex<mpsc::Receiver<RawWtStream>>,
    datagrams: Mutex<mpsc::Receiver<Vec<u8>>>,
    open_uni: Mutex<()>,
    open_bi: Mutex<()>,
    max_read: usize,
    max_datagram: usize,
    streams: Arc<Semaphore>,
    _permit: StdMutex<Option<OwnedSemaphorePermit>>,
}
impl SessionInner {
    fn cleanup(&self, connection: &quinn::Connection) {
        self.cancel.cancel();
        connection.close(0u32.into(), b"raw session ended");
        *self.final_stats.lock().unwrap_or_else(|e| e.into_inner()) = connection.stats();
        self.closed.send_replace(Some(
            self.local_close
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| {
                    close_info(
                        connection
                            .close_reason()
                            .unwrap_or(quinn::ConnectionError::LocallyClosed),
                    )
                }),
        ));
        let streams: Vec<_> = self
            .tracked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for stream in streams {
            if let Some(send) = &stream.send {
                if let Some(mut native) = send.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = native.reset_with_error_code(0);
                }
            }
            if let Some(recv) = &stream.recv {
                if let Some(mut native) = recv.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    native.stop_sending(0);
                }
            }
        }
        self._permit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}
struct SessionDriverCleanup {
    state: Arc<SessionInner>,
    connection: quinn::Connection,
}
impl Drop for SessionDriverCleanup {
    fn drop(&mut self) {
        self.state.cleanup(&self.connection);
    }
}
struct SessionTransport {
    session: Arc<Session>,
    connection: quinn::Connection,
}
#[derive(Clone, Debug, Serialize)]
pub struct RawWtSessionInfo {
    pub connection_id: u64,
    pub session_id: u64,
    pub peer_address: String,
    pub closed: bool,
    pub principal: Option<String>,
    pub attributes: HashMap<String, String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RawWtCloseInfo {
    pub kind: String,
    pub code: Option<u64>,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct RawWtStats {
    pub rtt_micros: u64,
    pub cwnd: u64,
    pub mtu: u16,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub congestion_events: u64,
    pub udp_tx_bytes: u64,
    pub udp_tx_packets: u64,
    pub udp_rx_bytes: u64,
    pub udp_rx_packets: u64,
    pub ping_tx: u64,
    pub ack_rx: u64,
}
impl RawWtSession {
    pub async fn accept_bi(&self) -> Result<RawWtStream, String> {
        self.accept(&self.inner.bi).await
    }
    pub async fn accept_uni(&self) -> Result<RawWtStream, String> {
        self.accept(&self.inner.uni).await
    }
    async fn accept(&self, rx: &Mutex<mpsc::Receiver<RawWtStream>>) -> Result<RawWtStream, String> {
        tokio::select! {biased;_=self.inner.cancel.cancelled()=>Err("session closed".into()),result=async{rx.lock().await.recv().await}=>result.ok_or_else(||"session closed".into())}
    }
    pub async fn open_bi(&self) -> Result<RawWtStream, String> {
        tokio::select! {biased;_=self.inner.cancel.cancelled()=>Err("session closed".into()),result=async{let _lock=self.inner.open_bi.lock().await;let transport=self.transport()?;let stream=transport.session.open_bi(transport.session.session_id()).await.map_err(|e|e.to_string())?;let(send,recv)=stream.split();RawWtStream::new(&self.inner,Some(send),Some(recv)).await}=>result}
    }
    pub async fn open_uni(&self) -> Result<RawWtStream, String> {
        tokio::select! {biased;_=self.inner.cancel.cancelled()=>Err("session closed".into()),result=async{let _lock=self.inner.open_uni.lock().await;let transport=self.transport()?;let send=transport.session.open_uni(transport.session.session_id()).await.map_err(|e|e.to_string())?;RawWtStream::new(&self.inner,Some(send),None).await}=>result}
    }
    pub fn send_datagram(&self, data: &[u8]) -> Result<(), String> {
        if self.inner.cancel.is_cancelled() {
            return Err("session closed".into());
        }
        if data.len() > self.max_datagram_size().unwrap_or(0) {
            return Err("datagram exceeds session limit".into());
        }
        self.transport()?
            .session
            .datagram_sender()
            .send_datagram(Bytes::copy_from_slice(data))
            .map_err(|e| e.to_string())
    }
    pub async fn receive_datagram(&self) -> Result<Vec<u8>, String> {
        tokio::select! {biased;_=self.inner.cancel.cancelled()=>Err("session closed".into()),result=async{self.inner.datagrams.lock().await.recv().await}=>result.ok_or_else(||"session closed".into())}
    }
    fn transport(&self) -> Result<Arc<SessionTransport>, String> {
        self.inner
            .transport
            .upgrade()
            .ok_or_else(|| "session closed".into())
    }
    pub fn max_datagram_size(&self) -> Option<usize> {
        let transport = self.inner.transport.upgrade()?;
        let sid = self.inner.info.session_id / 4;
        let overhead = if sid < 64 {
            1
        } else if sid < 16384 {
            2
        } else if sid < (1 << 30) {
            4
        } else {
            8
        };
        transport
            .connection
            .max_datagram_size()
            .map(|n| n.saturating_sub(overhead).min(self.inner.max_datagram))
    }
    pub fn info(&self) -> RawWtSessionInfo {
        let mut info = self.inner.info.clone();
        if let Some(t) = self.inner.transport.upgrade() {
            info.peer_address = t.connection.remote_address().to_string();
        }
        info.closed = self.inner.cancel.is_cancelled();
        info
    }
    pub fn stats(&self) -> RawWtStats {
        let s = self
            .inner
            .transport
            .upgrade()
            .map(|t| t.connection.stats())
            .unwrap_or_else(|| {
                *self
                    .inner
                    .final_stats
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
            });
        RawWtStats {
            rtt_micros: s.path.rtt.as_micros().min(u64::MAX as u128) as u64,
            cwnd: s.path.cwnd,
            mtu: s.path.current_mtu,
            sent_packets: s.path.sent_packets,
            lost_packets: s.path.lost_packets,
            lost_bytes: s.path.lost_bytes,
            congestion_events: s.path.congestion_events,
            udp_tx_bytes: s.udp_tx.bytes,
            udp_tx_packets: s.udp_tx.datagrams,
            udp_rx_bytes: s.udp_rx.bytes,
            udp_rx_packets: s.udp_rx.datagrams,
            ping_tx: s.frame_tx.ping,
            ack_rx: s.frame_rx.acks,
        }
    }
    pub fn close(&self, code: u32, reason: &[u8]) -> Result<(), String> {
        *self
            .inner
            .local_close
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(RawWtCloseInfo {
            kind: "local".into(),
            code: Some(code.into()),
            reason: String::from_utf8_lossy(reason).into(),
        });
        self.inner.cancel.cancel();
        if let Some(t) = self.inner.transport.upgrade() {
            t.connection.close(code.into(), reason);
        }
        Ok(())
    }
    pub async fn closed(&self) -> RawWtCloseInfo {
        let mut rx = self.inner.closed.subscribe();
        loop {
            if let Some(value) = rx.borrow().clone() {
                return value;
            }
            if rx.changed().await.is_err() {
                return RawWtCloseInfo {
                    kind: "connection".into(),
                    code: None,
                    reason: "session ended".into(),
                };
            }
        }
    }
}
fn close_info(error: quinn::ConnectionError) -> RawWtCloseInfo {
    match error {
        quinn::ConnectionError::ApplicationClosed(e) => RawWtCloseInfo {
            kind: "application".into(),
            code: Some(e.error_code.into_inner()),
            reason: String::from_utf8_lossy(&e.reason).into(),
        },
        other => RawWtCloseInfo {
            kind: "connection".into(),
            code: None,
            reason: other.to_string(),
        },
    }
}

#[derive(Clone)]
pub struct RawWtStream {
    inner: Arc<StreamInner>,
}
struct StreamInner {
    send: Option<StdMutex<Option<Send>>>,
    recv: Option<StdMutex<Option<Recv>>>,
    send_op: Mutex<()>,
    recv_op: Mutex<()>,
    send_cancel: CancellationToken,
    recv_cancel: CancellationToken,
    session_cancel: CancellationToken,
    id: u64,
    max_read: usize,
    _permit: OwnedSemaphorePermit,
}
#[derive(Clone, Debug, Serialize)]
pub struct RawWtStreamInfo {
    pub stream_id: u64,
    pub has_send: bool,
    pub has_receive: bool,
}
struct PendingStream {
    send: Option<Send>,
    recv: Option<Recv>,
}
impl Drop for PendingStream {
    fn drop(&mut self) {
        if let Some(send) = &mut self.send {
            let _ = send.reset_with_error_code(0);
        }
        if let Some(recv) = &mut self.recv {
            recv.stop_sending(0);
        }
    }
}
struct WriteGuard {
    inner: Arc<StreamInner>,
    complete: bool,
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.inner.send_cancel.cancel();
            if let Some(send) = &self.inner.send {
                if let Some(native) = send.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = native.reset_with_error_code(0);
                }
            }
        }
    }
}
impl RawWtStream {
    async fn new(
        session: &SessionInner,
        send: Option<Send>,
        recv: Option<Recv>,
    ) -> Result<Self, String> {
        let mut pending = PendingStream { send, recv };
        let permit = session
            .streams
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| e.to_string())?;
        let send = pending.send.take();
        let recv = pending.recv.take();
        let id = send
            .as_ref()
            .map(|s| s.send_id().into_inner())
            .or_else(|| recv.as_ref().map(|r| r.recv_id().into_inner()))
            .unwrap_or(0);
        let inner = Arc::new(StreamInner {
            send: send.map(|s| StdMutex::new(Some(s))),
            recv: recv.map(|r| StdMutex::new(Some(r))),
            send_op: Mutex::new(()),
            recv_op: Mutex::new(()),
            send_cancel: CancellationToken::new(),
            recv_cancel: CancellationToken::new(),
            session_cancel: session.cancel.clone(),
            id,
            max_read: session.max_read,
            _permit: permit,
        });
        let mut tracked = session.tracked.lock().unwrap_or_else(|e| e.into_inner());
        tracked.retain(|weak| weak.strong_count() > 0);
        tracked.push(Arc::downgrade(&inner));
        Ok(Self { inner })
    }
    pub fn info(&self) -> RawWtStreamInfo {
        RawWtStreamInfo {
            stream_id: self.inner.id,
            has_send: self.inner.send.is_some(),
            has_receive: self.inner.recv.is_some(),
        }
    }
    /// Maximum bytes accepted by a single write, checked before foreign copies.
    pub fn max_write_size(&self) -> usize {
        self.inner.max_read
    }
    /// Cancellation leaves unread bytes in the transport. An empty result is EOF.
    pub async fn read(&self, max: usize) -> Result<Vec<u8>, String> {
        if max == 0 || max > self.inner.max_read {
            return Err("read exceeds configured limit".into());
        }
        let recv = self
            .inner
            .recv
            .as_ref()
            .ok_or("stream has no receive direction")?;
        tokio::select! {biased;_=self.inner.session_cancel.cancelled()=>Err("session closed".into()),_=self.inner.recv_cancel.cancelled()=>Err("receive stopped".into()),result=async{
            let _operation=self.inner.recv_op.lock().await;
            let mut bytes=vec![0;max];let mut buf=ReadBuf::new(&mut bytes);
            futures_util::future::poll_fn(|cx|{let mut guard=recv.lock().unwrap_or_else(|e|e.into_inner());let Some(native)=guard.as_mut()else{return std::task::Poll::Ready(Err(std::io::Error::other("session closed")));};std::pin::Pin::new(native).poll_read(cx,&mut buf)}).await.map_err(|e|e.to_string())?;
            let len=buf.filled().len();bytes.truncate(len);Ok(bytes)
        }=>result}
    }
    /// Writes are serialized and bounded by `max_read_bytes`. Cancelling a
    /// partial write resets that send direction, preventing reuse of partial media.
    pub async fn write(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > self.inner.max_read {
            return Err("write exceeds configured limit".into());
        }
        let send = self
            .inner
            .send
            .as_ref()
            .ok_or("stream has no send direction")?;
        tokio::select! {biased;_=self.inner.session_cancel.cancelled()=>Err("session closed".into()),_=self.inner.send_cancel.cancelled()=>Err("send reset".into()),result=async{
            let _operation=self.inner.send_op.lock().await;let mut guard=WriteGuard{inner:self.inner.clone(),complete:false};let mut offset=0;
            futures_util::future::poll_fn(|cx|{let mut guard=send.lock().unwrap_or_else(|e|e.into_inner());let Some(native)=guard.as_mut()else{return std::task::Poll::Ready(Err(std::io::Error::other("session closed")));};while offset<bytes.len(){match std::pin::Pin::new(&mut *native).poll_write(cx,&bytes[offset..]){std::task::Poll::Ready(Ok(0))=>return std::task::Poll::Ready(Err(std::io::ErrorKind::WriteZero.into())),std::task::Poll::Ready(Ok(n))=>offset+=n,std::task::Poll::Ready(Err(e))=>return std::task::Poll::Ready(Err(e)),std::task::Poll::Pending=>return std::task::Poll::Pending}}std::task::Poll::Ready(Ok(()))}).await.map_err(|e|e.to_string())?;guard.complete=true;Ok(())
        }=>result}
    }
    pub async fn finish(&self) -> Result<(), String> {
        let send = self
            .inner
            .send
            .as_ref()
            .ok_or("stream has no send direction")?;
        tokio::select! {biased;_=self.inner.session_cancel.cancelled()=>Err("session closed".into()),_=self.inner.send_cancel.cancelled()=>Err("send reset".into()),result=async{let _operation=self.inner.send_op.lock().await;futures_util::future::poll_fn(|cx|{let mut guard=send.lock().unwrap_or_else(|e|e.into_inner());let Some(native)=guard.as_mut()else{return std::task::Poll::Ready(Err(std::io::Error::other("session closed")));};std::pin::Pin::new(native).poll_shutdown(cx)}).await.map_err(|e|e.to_string())}=>result}
    }
    pub async fn reset(&self, code: u32) -> Result<(), String> {
        self.inner.send_cancel.cancel();
        self.inner
            .send
            .as_ref()
            .ok_or("stream has no send direction")?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .ok_or("session closed")?
            .reset_with_error_code(code.into())
            .map_err(|e| e.to_string())
    }
    pub async fn stop(&self, code: u32) -> Result<(), String> {
        self.inner.recv_cancel.cancel();
        self.inner
            .recv
            .as_ref()
            .ok_or("stream has no receive direction")?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .ok_or("session closed")?
            .stop_sending(code.into());
        Ok(())
    }
    pub async fn set_priority(&self, priority: i32) -> Result<(), String> {
        self.inner
            .send
            .as_ref()
            .ok_or("stream has no send direction")?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or("session closed")?
            .set_priority(priority)
            .map_err(|e| e.to_string())
    }
    pub async fn priority(&self) -> Result<Option<i32>, String> {
        self.inner
            .send
            .as_ref()
            .ok_or("stream has no send direction")?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or("session closed")?
            .priority()
            .map_err(|e| e.to_string())
    }
    pub async fn transport_acked(&self) -> Result<bool, String> {
        let ack = self
            .inner
            .send
            .as_ref()
            .ok_or("stream has no send direction")?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .ok_or("session closed")?
            .transport_acked();
        tokio::select! {biased;_=self.inner.session_cancel.cancelled()=>Ok(false),_=self.inner.send_cancel.cancelled()=>Ok(false),value=ack=>Ok(value)}
    }
}
impl Drop for StreamInner {
    fn drop(&mut self) {
        if let Some(send) = &mut self.send {
            if let Some(mut native) = send.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = native.reset_with_error_code(0);
            }
        }
        if let Some(recv) = &mut self.recv {
            if let Some(mut native) = recv.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
                native.stop_sending(0);
            }
        }
    }
}
