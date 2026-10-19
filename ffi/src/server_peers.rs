//! Peer events (ABI 7.8): which peers are connected to a dispatch server
//! (`aster_server_start`) and when that changes. Rust
//! `aster::rpc::ServerHandle::peer_events`; semantics in `aster::rpc::peers`.
//!
//! ```text
//!   aster_server_peer_events(server, &receiver, &snapshot_json)   snapshot + subscribe, atomically
//!   aster_peer_events_poll(rt, receiver, timeout_ms, &event_json) one event, or TIMEOUT
//!   aster_peer_events_release(receiver)                           wakes a pending poll
//! ```
//!
//! Peer ids and ALPNs in the JSON are strings; every JSON buffer is owned by
//! the caller and released with `aster_string_release`.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aster::rpc::{DisconnectReason, PeerEvent, PeerEventReceiver, PeerIdentity, PeerSnapshot};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::server::{aster_server_t, SERVERS};
use crate::{
    alloc_owned_bytes, aster_bytes_t, aster_runtime_t, aster_status_t, load_runtime,
    set_last_error, HandleRegistry,
};

/// One subscription: the broadcast receiver, serialised, and a close flag
/// `aster_peer_events_release` raises to end a pending poll.
pub(crate) struct PeerEventsState {
    receiver: tokio::sync::Mutex<PeerEventReceiver>,
    closed: AtomicBool,
    close_notify: tokio::sync::Notify,
}

pub(crate) static PEER_EVENT_RECEIVERS: once_cell::sync::Lazy<HandleRegistry<PeerEventsState>> =
    once_cell::sync::Lazy::new(HandleRegistry::new);

fn identity_json(identity: &PeerIdentity) -> Value {
    json!({
        "endpoint_id": identity.endpoint_id,
        "principal": identity.principal,
        "roles": identity.roles,
    })
}

fn alpn_string(alpn: &[u8]) -> String {
    String::from_utf8_lossy(alpn).into_owned()
}

fn reason_name(reason: DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::Closed => "closed",
        DisconnectReason::TimedOut => "timed_out",
        DisconnectReason::Revoked => "revoked",
        DisconnectReason::Drained => "drained",
        DisconnectReason::Error => "error",
    }
}

pub(crate) fn snapshot_json(snapshot: &[PeerSnapshot]) -> Vec<u8> {
    let peers: Vec<Value> = snapshot
        .iter()
        .map(|p| {
            json!({
                "peer": p.peer,
                "alpn": alpn_string(&p.alpn),
                "connections": p.connections,
                "identity": identity_json(&p.identity),
            })
        })
        .collect();
    serde_json::to_vec(&peers).unwrap_or_default()
}

pub(crate) fn event_json(event: &PeerEvent) -> Vec<u8> {
    let value = match event {
        PeerEvent::Connected {
            peer,
            alpn,
            identity,
        } => json!({
            "type": "connected",
            "peer": peer,
            "alpn": alpn_string(alpn),
            "identity": identity_json(identity),
        }),
        PeerEvent::Disconnected { peer, reason } => json!({
            "type": "disconnected",
            "peer": peer,
            "reason": reason_name(*reason),
        }),
    };
    serde_json::to_vec(&value).unwrap_or_default()
}

fn lagged_json(missed: u64) -> Vec<u8> {
    serde_json::to_vec(&json!({"type": "lagged", "missed": missed})).unwrap_or_default()
}

/// ABI 7.8: the peers connected to `server` now, and a receiver of every
/// connect and disconnect after them, taken together so no peer falls
/// between the two. `*out_receiver` is the receiver handle (release it with
/// `aster_peer_events_release`); `*out_snapshot_json` is owned JSON
/// (`aster_string_release`): an array of `{"peer", "alpn", "connections",
/// "identity": {"endpoint_id", "principal" (string or null), "roles": []}}`.
///
/// Events are per peer, not per connection, and only after the node's
/// admission gates: a peer enrollment (or a drain) refuses never appears. A
/// peer is connected from its first admitted `aster/1` connection until its
/// last one ends. `NOT_FOUND` for an unknown server; `ALREADY_CLOSED` for one
/// that is stopping.
///
/// # Safety
/// `out_receiver` and `out_snapshot_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_server_peer_events(
    server: aster_server_t,
    out_receiver: *mut u64,
    out_snapshot_json: *mut aster_bytes_t,
) -> i32 {
    if out_receiver.is_null() || out_snapshot_json.is_null() {
        set_last_error("aster_server_peer_events: an output pointer is NULL");
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    // SAFETY: checked non-null; the caller passes writable storage.
    unsafe {
        ptr::write(out_receiver, 0);
        ptr::write(
            out_snapshot_json,
            aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
        );
    }
    let Some(state) = SERVERS.get(server) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    let subscribed = state
        .handle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|handle| handle.peer_events());
    let Some((snapshot, receiver)) = subscribed else {
        set_last_error("aster_server_peer_events: the server is stopping");
        return aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32;
    };
    let handle = PEER_EVENT_RECEIVERS.insert(PeerEventsState {
        receiver: tokio::sync::Mutex::new(receiver),
        closed: AtomicBool::new(false),
        close_notify: tokio::sync::Notify::new(),
    });
    // SAFETY: checked non-null above.
    unsafe {
        ptr::write(out_receiver, handle);
        ptr::write(
            out_snapshot_json,
            alloc_owned_bytes(snapshot_json(&snapshot)),
        );
    }
    aster_status_t::ASTER_STATUS_OK as i32
}

/// ABI 7.8: wait up to `timeout_ms` (0: don't wait) for the next peer event
/// and write it as owned JSON (`aster_string_release`):
/// `{"type": "connected", "peer", "alpn", "identity": {...}}`,
/// `{"type": "disconnected", "peer", "reason": "closed" | "timed_out" |
/// "revoked" | "drained" | "error"}` or `{"type": "lagged", "missed": n}`.
///
/// `lagged` means the receiver fell behind a bounded queue (1024 events) and
/// `missed` events were dropped; its state is stale, so call
/// `aster_server_peer_events` again, release this receiver and rebuild from
/// the new snapshot. `TIMEOUT`: no event yet (`*out_json` stays empty).
/// `NOT_FOUND`: the receiver was released. `ALREADY_CLOSED`: the server
/// ended, so no event will come. Polls on one receiver are serialised; a
/// release wakes a pending poll with `NOT_FOUND`. `UNSUPPORTED` on a tokio
/// runtime thread.
///
/// # Safety
/// `out_json` must be writable.
#[no_mangle]
pub unsafe extern "C" fn aster_peer_events_poll(
    runtime: aster_runtime_t,
    receiver: u64,
    timeout_ms: u32,
    out_json: *mut aster_bytes_t,
) -> i32 {
    if out_json.is_null() {
        set_last_error("aster_peer_events_poll: out_json is NULL");
        return aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
    }
    // SAFETY: checked non-null.
    unsafe {
        ptr::write(
            out_json,
            aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
        );
    }
    if let Some(error) = crate::http_server::blocking_on_runtime("aster_peer_events_poll") {
        return error;
    }
    let bridge = match load_runtime(runtime) {
        Ok(bridge) => bridge,
        Err(status) => return status as i32,
    };
    let Some(state) = PEER_EVENT_RECEIVERS.get(receiver) else {
        return aster_status_t::ASTER_STATUS_NOT_FOUND as i32;
    };
    enum Outcome {
        Event(Vec<u8>),
        Timeout,
        Released,
        Ended,
    }
    let outcome = bridge.runtime.block_on(async {
        let closed = state.close_notify.notified();
        tokio::pin!(closed);
        // `notify_waiters` keeps no permit: register, then test the flag.
        closed.as_mut().enable();
        if state.closed.load(Ordering::Acquire) {
            return Outcome::Released;
        }
        let mut receiver = tokio::select! {
            biased;
            _ = &mut closed => return Outcome::Released,
            guard = state.receiver.lock() => guard,
        };
        let wait = async {
            if timeout_ms == 0 {
                match receiver.try_recv() {
                    Ok(event) => Some(Ok(event)),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => None,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                        Some(Err(RecvError::Lagged(n)))
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        Some(Err(RecvError::Closed))
                    }
                }
            } else {
                tokio::time::timeout(
                    Duration::from_millis(u64::from(timeout_ms)),
                    receiver.recv(),
                )
                .await
                .ok()
            }
        };
        let result = tokio::select! {
            biased;
            _ = &mut closed => return Outcome::Released,
            result = wait => result,
        };
        match result {
            None => Outcome::Timeout,
            Some(Ok(event)) => Outcome::Event(event_json(&event)),
            Some(Err(RecvError::Lagged(missed))) => Outcome::Event(lagged_json(missed)),
            Some(Err(RecvError::Closed)) => Outcome::Ended,
        }
    });
    match outcome {
        Outcome::Event(json) => {
            // SAFETY: checked non-null above.
            unsafe { ptr::write(out_json, alloc_owned_bytes(json)) };
            aster_status_t::ASTER_STATUS_OK as i32
        }
        Outcome::Timeout => aster_status_t::ASTER_STATUS_TIMEOUT as i32,
        Outcome::Released => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
        Outcome::Ended => aster_status_t::ASTER_STATUS_ALREADY_CLOSED as i32,
    }
}

/// ABI 7.8: release a peer event receiver. A pending
/// `aster_peer_events_poll` on it returns `NOT_FOUND`. `NOT_FOUND` for an
/// unknown or already released receiver.
#[no_mangle]
pub extern "C" fn aster_peer_events_release(receiver: u64) -> i32 {
    match PEER_EVENT_RECEIVERS.remove(receiver) {
        Some(state) => {
            state.closed.store(true, Ordering::Release);
            state.close_notify.notify_waiters();
            aster_status_t::ASTER_STATUS_OK as i32
        }
        None => aster_status_t::ASTER_STATUS_NOT_FOUND as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_encode_as_documented() {
        let identity = PeerIdentity {
            endpoint_id: "ab".into(),
            principal: None,
            roles: vec!["ops".into()],
        };
        let connected: Value = serde_json::from_slice(&event_json(&PeerEvent::Connected {
            peer: "ab".into(),
            alpn: b"aster/1".to_vec(),
            identity: identity.clone(),
        }))
        .unwrap();
        assert_eq!(
            connected,
            json!({"type": "connected", "peer": "ab", "alpn": "aster/1",
                   "identity": {"endpoint_id": "ab", "principal": null, "roles": ["ops"]}})
        );
        for (reason, name) in [
            (DisconnectReason::Closed, "closed"),
            (DisconnectReason::TimedOut, "timed_out"),
            (DisconnectReason::Revoked, "revoked"),
            (DisconnectReason::Drained, "drained"),
            (DisconnectReason::Error, "error"),
        ] {
            let gone: Value = serde_json::from_slice(&event_json(&PeerEvent::Disconnected {
                peer: "ab".into(),
                reason,
            }))
            .unwrap();
            assert_eq!(
                gone,
                json!({"type": "disconnected", "peer": "ab", "reason": name})
            );
        }
        let lagged: Value = serde_json::from_slice(&lagged_json(3)).unwrap();
        assert_eq!(lagged, json!({"type": "lagged", "missed": 3}));
        let snapshot: Value = serde_json::from_slice(&snapshot_json(&[PeerSnapshot {
            peer: "ab".into(),
            alpn: b"aster/1".to_vec(),
            identity,
            connections: 2,
        }]))
        .unwrap();
        assert_eq!(snapshot[0]["connections"], 2);
    }

    #[test]
    fn unknown_receivers_and_servers_are_not_found() {
        assert_eq!(
            aster_peer_events_release(u64::MAX),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        let mut receiver = 0u64;
        let mut snapshot = aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        };
        let status = unsafe { aster_server_peer_events(u64::MAX, &mut receiver, &mut snapshot) };
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);
        assert_eq!(receiver, 0);
    }
}
