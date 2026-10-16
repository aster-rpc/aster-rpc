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
//! A connection-scoped session also belongs to the principal that opened it:
//! the forwarded user (`iss|sub`) when the opening call carried an
//! `aster-on-behalf-of` entry, else the caller (an authenticator's principal,
//! or the peer). A gateway edge multiplexes many users over one connection,
//! so a later call on the session from any other principal, the edge itself
//! included, is refused with `PERMISSION_DENIED` rather than run in the
//! opener's instance.
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
pub(crate) type SessionFactory =
    Arc<dyn Fn() -> Result<SessionInstance, SessionError> + Send + Sync>;

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
    /// The session belongs to another principal on the same connection.
    NotOwner(String),
}

impl SessionError {
    pub(crate) fn status(&self) -> RpcStatus {
        match self {
            SessionError::ScopeMismatch(m) => RpcStatus::error(StatusCode::FailedPrecondition, m),
            SessionError::NotFound(m) => RpcStatus::error(StatusCode::NotFound, m),
            SessionError::Exhausted(m) => RpcStatus::error(StatusCode::ResourceExhausted, m),
            SessionError::NotOwner(m) => RpcStatus::error(StatusCode::PermissionDenied, m),
        }
    }
}

/// Per-connection session state.
#[derive(Default)]
struct ConnectionState {
    /// Highest session id ever opened on this connection. Ids at or below it
    /// that are not active belong to closed sessions (the graveyard).
    last_opened: i32,
    active: HashMap<(i32, ServiceKey), OpenSession>,
}

/// A live connection-scoped session and the principal it belongs to.
struct OpenSession {
    instance: SessionInstance,
    /// `None` until the opening call's principal is known: a dispatcher with
    /// an authenticator resolves the session before authenticating, then
    /// [`SessionStore::claim`]s it.
    owner: Option<String>,
}

fn not_owner(session_id: i32) -> SessionError {
    SessionError::NotOwner(format!(
        "session {session_id} belongs to another principal on this connection"
    ))
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
    /// already be known to be non-zero. `principal` is the caller's identity
    /// when it is already known (no authenticator): a new session records it
    /// and an existing one owned by anyone else is refused. With `None` the
    /// caller must [`claim`](Self::claim) the session once it knows.
    ///
    /// The factory runs under the store lock. Factories are synchronous
    /// constructors, and holding the lock is what makes "check the cap, bump
    /// `last_opened`, insert" one step.
    pub(crate) fn resolve_connection(
        &self,
        connection_id: u64,
        service: &ServiceKey,
        session_id: i32,
        principal: Option<&str>,
        factory: &SessionFactory,
    ) -> Result<SessionInstance, SessionError> {
        let mut conns = self.connections.lock().unwrap();
        let state = conns.entry(connection_id).or_default();
        let key = (session_id, service.clone());
        if let Some(open) = state.active.get_mut(&key) {
            if let Some(principal) = principal {
                check_owner(open, principal, session_id)?;
            }
            return Ok(open.instance.clone());
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
        let inst = factory()?;
        state.last_opened = session_id;
        state.active.insert(
            key,
            OpenSession {
                instance: inst.clone(),
                owner: principal.map(str::to_string),
            },
        );
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

    /// Bind a session resolved with no principal to `principal`, or check
    /// that it already belongs to it. A session reaped since resolution (its
    /// connection closed) has nothing to claim and passes.
    pub(crate) fn claim(
        &self,
        connection_id: u64,
        service: &ServiceKey,
        session_id: i32,
        principal: &str,
    ) -> Result<(), SessionError> {
        let mut conns = self.connections.lock().unwrap();
        let Some(open) = conns
            .get_mut(&connection_id)
            .and_then(|state| state.active.get_mut(&(session_id, service.clone())))
        else {
            return Ok(());
        };
        check_owner(open, principal, session_id)
    }

    /// Whether session `session_id` of `service` is live on `connection_id`.
    pub(crate) fn has_session(
        &self,
        connection_id: u64,
        service: &ServiceKey,
        session_id: i32,
    ) -> bool {
        self.connections
            .lock()
            .unwrap()
            .get(&connection_id)
            .is_some_and(|state| state.active.contains_key(&(session_id, service.clone())))
    }

    /// Whether any session is live on `connection_id`.
    pub(crate) fn has_connection_sessions(&self, connection_id: u64) -> bool {
        self.connections
            .lock()
            .unwrap()
            .get(&connection_id)
            .is_some_and(|state| !state.active.is_empty())
    }

    /// Lookup-or-create for an HTTP-style call: string id, scoped to
    /// `principal`. No graveyard, no cap (see the module docs).
    pub(crate) fn resolve_http(
        &self,
        principal: &str,
        service: &ServiceKey,
        session_id: &str,
        factory: &SessionFactory,
    ) -> Result<SessionInstance, SessionError> {
        let key = (
            principal.to_string(),
            service.0.clone(),
            service.1,
            session_id.to_string(),
        );
        let mut sessions = self.http.lock().unwrap();
        if let Some(instance) = sessions.get(&key) {
            return Ok(instance.clone());
        }
        let instance = factory()?;
        tracing::debug!(
            target: SESSION,
            principal,
            session_id,
            service = service.0.as_str(),
            version = service.1,
            "http session opened"
        );
        sessions.insert(key, instance.clone());
        Ok(instance)
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

    /// Drop every live instance of the services in `keys`, on every
    /// connection and every HTTP principal. Connection graveyards are kept,
    /// so a closed id still answers `NOT_FOUND` rather than reopening. Used
    /// when a late-activated RPC surface is withdrawn: its session instances
    /// must not outlive it, and must not be reused by a later surface that
    /// serves the same `(name, version)`. Returns how many were dropped.
    pub(crate) fn drop_services(&self, keys: &[ServiceKey]) -> usize {
        if keys.is_empty() {
            return 0;
        }
        let mut dropped: Vec<SessionInstance> = Vec::new();
        {
            let mut connections = self.connections.lock().unwrap();
            for state in connections.values_mut() {
                let stale: Vec<(i32, ServiceKey)> = state
                    .active
                    .keys()
                    .filter(|(_, key)| keys.contains(key))
                    .cloned()
                    .collect();
                dropped.extend(
                    stale
                        .into_iter()
                        .filter_map(|k| state.active.remove(&k))
                        .map(|open| open.instance),
                );
            }
        }
        {
            let mut http = self.http.lock().unwrap();
            let stale: Vec<HttpKey> = http
                .keys()
                .filter(|(_, name, version, _)| {
                    keys.iter().any(|k| k.0 == *name && k.1 == *version)
                })
                .cloned()
                .collect();
            dropped.extend(stale.into_iter().filter_map(|k| http.remove(&k)));
        }
        // Instances (and any `Drop` they run) are released outside the locks.
        dropped.len()
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

/// First principal to claim an unowned session gets it; anyone else is refused.
fn check_owner(
    open: &mut OpenSession,
    principal: &str,
    session_id: i32,
) -> Result<(), SessionError> {
    match &open.owner {
        Some(owner) if owner == principal => Ok(()),
        Some(_) => Err(not_owner(session_id)),
        None => {
            open.owner = Some(principal.to_string());
            Ok(())
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NoService;

    #[async_trait::async_trait]
    impl ServiceDispatch for NoService {
        fn name(&self) -> &str {
            "test.Counter"
        }
        fn version(&self) -> i32 {
            1
        }
        async fn dispatch(&self, _method: &str, _call: crate::rpc::server::Call) {}
    }

    #[test]
    fn failed_factory_does_not_cache_or_advance_session_id() {
        let store = SessionStore::new(4);
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = attempts.clone();
        let factory: SessionFactory = Arc::new(move || {
            seen.fetch_add(1, Ordering::Relaxed);
            Err(SessionError::Exhausted("dispatch full".into()))
        });
        let service = ("test.Counter".to_string(), 1);

        assert!(matches!(
            store.resolve_connection(7, &service, 3, Some("alice"), &factory),
            Err(SessionError::Exhausted(_))
        ));
        assert!(matches!(
            store.resolve_connection(7, &service, 3, Some("alice"), &factory),
            Err(SessionError::Exhausted(_))
        ));
        assert_eq!(attempts.load(Ordering::Relaxed), 2, "retry reruns factory");
        let connections = store.connections.lock().unwrap();
        let state = connections.get(&7).unwrap();
        assert_eq!(state.last_opened, 0);
        assert!(state.active.is_empty());
    }

    #[test]
    fn a_session_belongs_to_the_principal_that_opened_it() {
        let store = SessionStore::new(4);
        let factory: SessionFactory = Arc::new(|| Err(SessionError::Exhausted("unused".into())));
        let service = ("test.Counter".to_string(), 1);
        // Seed a live session owned by alice without running a factory.
        let opened: SessionFactory = Arc::new(|| Ok(SessionInstance::Native(Arc::new(NoService))));
        store
            .resolve_connection(7, &service, 3, Some("alice"), &opened)
            .unwrap();
        assert!(store
            .resolve_connection(7, &service, 3, Some("alice"), &factory)
            .is_ok());
        assert!(matches!(
            store.resolve_connection(7, &service, 3, Some("bob"), &factory),
            Err(SessionError::NotOwner(_))
        ));
        // Resolved before authentication, claimed after.
        store
            .resolve_connection(7, &service, 4, None, &opened)
            .unwrap();
        store.claim(7, &service, 4, "carol").unwrap();
        assert!(matches!(
            store.claim(7, &service, 4, "alice"),
            Err(SessionError::NotOwner(_))
        ));
        assert!(matches!(
            store.resolve_connection(7, &service, 4, Some("alice"), &factory),
            Err(SessionError::NotOwner(_))
        ));
    }
}
