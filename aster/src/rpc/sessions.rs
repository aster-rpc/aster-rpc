//! Session-scoped service instances (Aster-multiplexed-streams §7.5).
//!
//! Over Iroh, sessions are scoped to the QUIC connection they were opened on:
//! the key is `(connection_id, sessionId, service)`, never the session id
//! alone, so a peer cannot reach another connection's session by presenting
//! its id. Each connection tracks a graveyard high-water mark
//! (`last_opened`) and a cap on active sessions, and the whole entry is
//! reaped when the connection closes. These are the rules Python
//! (`AsterServer._resolve_instance`) and Java (`AsterServer.resolveInstance`)
//! implement.
//!
//! HTTP-style transports (Salvo HTTP, WebTransport) hand the dispatcher no
//! connection id. Their sessions keep the string `aster-session-id` key, but
//! scoped to the caller's principal: the [`Authenticator`] principal when one
//! is configured, else the transport's peer id (for Salvo, the remote socket
//! address, which makes unauthenticated HTTP sessions per TCP connection in
//! practice). They have no graveyard or cap and live until
//! [`Dispatcher::end_session`] removes them.
//!
//! [`Authenticator`]: super::auth::Authenticator
//! [`Dispatcher::end_session`]: super::server::Dispatcher::end_session

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::codec::RpcStatus;
use super::foreign::ForeignSession;
use super::server::ServiceDispatch;
use super::status::StatusCode;
use crate::log_targets::SESSION;

/// `(service, version)`.
pub(crate) type ServiceKey = (String, i32);

/// One live session: a native per-session instance, or the Rust-side record
/// of a foreign (binding-implemented) session, whose state lives in the
/// binding under its `session_key`. Both kinds share one per-connection
/// table, so the graveyard and the cap span every service on a connection,
/// as in Python and Java.
#[derive(Clone)]
pub(crate) enum SessionInstance {
    Native(Arc<dyn ServiceDispatch>),
    Foreign(Arc<ForeignSession>),
}

/// Builds a fresh per-session instance of a session-scoped service.
pub(crate) type SessionFactory = Arc<dyn Fn() -> SessionInstance + Send + Sync>;

/// Default cap on active sessions per connection (`max_sessions_per_connection`).
pub const DEFAULT_MAX_SESSIONS_PER_CONNECTION: usize = 1024;

/// Why a session lookup was refused. Maps 1:1 onto a trailer status.
#[derive(Debug)]
pub(crate) enum SessionError {
    /// SHARED service with a session id, or SESSION service without one.
    ScopeMismatch(String),
    /// The id is at or below the connection's high-water mark and not active.
    NotFound(String),
    /// Opening the session would exceed `max_sessions_per_connection`.
    Exhausted(String),
}

impl SessionError {
    pub(crate) fn status(&self) -> RpcStatus {
        match self {
            SessionError::ScopeMismatch(m) => RpcStatus::error(StatusCode::FailedPrecondition, m),
            SessionError::NotFound(m) => RpcStatus::error(StatusCode::NotFound, m),
            SessionError::Exhausted(m) => RpcStatus::error(StatusCode::ResourceExhausted, m),
        }
    }
}

/// Per-connection session state.
#[derive(Default)]
struct ConnectionState {
    /// Highest session id ever opened on this connection. Ids at or below it
    /// that are not active belong to closed sessions (the graveyard).
    last_opened: i32,
    active: HashMap<(i32, ServiceKey), SessionInstance>,
}

/// `(principal, service, version, aster-session-id)`.
type HttpKey = (String, String, i32, String);

/// All live session instances for one [`Dispatcher`](super::server::Dispatcher).
pub(crate) struct SessionStore {
    max_per_connection: usize,
    connections: Mutex<HashMap<u64, ConnectionState>>,
    http: Mutex<HashMap<HttpKey, SessionInstance>>,
}

impl SessionStore {
    pub(crate) fn new(max_per_connection: usize) -> Self {
        Self {
            max_per_connection,
            connections: Mutex::default(),
            http: Mutex::default(),
        }
    }

    /// Lookup-or-create for a connection-scoped call (§7.5). `session_id` must
    /// already be known to be non-zero.
    ///
    /// The factory runs under the store lock. Factories are synchronous
    /// constructors, and holding the lock is what makes "check the cap, bump
    /// `last_opened`, insert" one step.
    pub(crate) fn resolve_connection(
        &self,
        connection_id: u64,
        service: &ServiceKey,
        session_id: i32,
        factory: &SessionFactory,
    ) -> Result<SessionInstance, SessionError> {
        let mut conns = self.connections.lock().unwrap();
        let state = conns.entry(connection_id).or_default();
        let key = (session_id, service.clone());
        if let Some(inst) = state.active.get(&key) {
            return Ok(inst.clone());
        }
        if session_id <= state.last_opened {
            return Err(SessionError::NotFound(format!(
                "session {session_id} was previously opened on this connection and is now closed"
            )));
        }
        // The cap counts active sessions only. A refused id does not move
        // `last_opened`, so retrying it reports RESOURCE_EXHAUSTED again rather
        // than NOT_FOUND.
        if state.active.len() >= self.max_per_connection {
            return Err(SessionError::Exhausted(format!(
                "session limit reached (max_sessions_per_connection={})",
                self.max_per_connection
            )));
        }
        let inst = factory();
        state.last_opened = session_id;
        state.active.insert(key, inst.clone());
        tracing::debug!(
            target: SESSION,
            connection_id,
            session_id,
            service = service.0.as_str(),
            version = service.1,
            active = state.active.len(),
            "session opened"
        );
        Ok(inst)
    }

    /// Lookup-or-create for an HTTP-style call: string id, scoped to
    /// `principal`. No graveyard, no cap (see the module docs).
    pub(crate) fn resolve_http(
        &self,
        principal: &str,
        service: &ServiceKey,
        session_id: &str,
        factory: &SessionFactory,
    ) -> SessionInstance {
        let key = (
            principal.to_string(),
            service.0.clone(),
            service.1,
            session_id.to_string(),
        );
        self.http
            .lock()
            .unwrap()
            .entry(key)
            .or_insert_with(|| {
                tracing::debug!(
                    target: SESSION,
                    principal,
                    session_id,
                    service = service.0.as_str(),
                    version = service.1,
                    "http session opened"
                );
                factory()
            })
            .clone()
    }

    /// Drop every session opened on `connection_id`, and the connection's
    /// graveyard and counters. Returns how many sessions were reaped.
    pub(crate) fn close_connection(&self, connection_id: u64) -> usize {
        let state = self.connections.lock().unwrap().remove(&connection_id);
        // Instances (and any `Drop` they run) are released outside the lock.
        let reaped = state.map_or(0, |s| s.active.len());
        if reaped > 0 {
            tracing::debug!(
                target: SESSION,
                connection_id,
                reaped,
                "sessions reaped with their connection"
            );
        }
        reaped
    }

    /// Drop every HTTP-scoped instance for `session_id`, for any principal.
    pub(crate) fn end_http_session(&self, session_id: &str) {
        let removed: Vec<_> = {
            let mut http = self.http.lock().unwrap();
            let keys: Vec<HttpKey> = http
                .keys()
                .filter(|(_, _, _, sid)| sid == session_id)
                .cloned()
                .collect();
            keys.into_iter().filter_map(|k| http.remove(&k)).collect()
        };
        tracing::debug!(
            target: SESSION,
            session_id,
            ended = removed.len(),
            "http session ended"
        );
        drop(removed);
    }
}

/// Whether a SHARED service may take this call: it must not carry a session id.
pub(crate) fn check_shared(service: &str, session_id: i32) -> Result<(), SessionError> {
    if session_id != 0 {
        return Err(SessionError::ScopeMismatch(format!(
            "service '{service}' is SHARED but call carried sessionId={session_id}"
        )));
    }
    Ok(())
}

/// The scope-mismatch error for a SESSION service called without a session id.
pub(crate) fn missing_session(service: &str) -> SessionError {
    SessionError::ScopeMismatch(format!(
        "service '{service}' is SESSION-scoped; call must carry a non-zero sessionId"
    ))
}
