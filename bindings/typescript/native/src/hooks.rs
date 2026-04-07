//! Hooks module — connection-level gating (before/after connect).
//!
//! The hook system uses channels from CoreHookReceiver. The JS side
//! receives hook invocations and must respond (allow/deny).

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Mutex;

use aster_transport_core::{CoreAfterHandshakeDecision, CoreHookReceiver};

/// Hook receiver for connection-level gating.
/// Call `takeHookReceiver()` on IrohNode to get this.
#[napi]
pub struct NodeHookReceiver {
    pub(crate) inner: Mutex<Option<CoreHookReceiver>>,
}

impl NodeHookReceiver {
    pub fn from_core(inner: CoreHookReceiver) -> Self {
        Self {
            inner: Mutex::new(Some(inner)),
        }
    }
}

/// Info about an incoming connection (before_connect hook).
#[napi(object)]
pub struct HookConnectInfo {
    pub remote_endpoint_id: String,
    pub alpn: Vec<u8>,
}

/// Info about a completed handshake (after_handshake hook).
#[napi(object)]
pub struct HookHandshakeInfo {
    pub remote_endpoint_id: String,
    pub alpn: Vec<u8>,
    pub is_alive: bool,
}

/// Result of waiting for a hook event.
#[napi(object)]
pub struct HookConnectEvent {
    pub info: HookConnectInfo,
    /// Internal ID for responding to this event.
    pub event_id: u32,
}

#[napi(object)]
pub struct HookHandshakeEvent {
    pub info: HookHandshakeInfo,
    pub event_id: u32,
}

// Store pending reply channels
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use tokio::sync::oneshot;

static NEXT_EVENT_ID: AtomicU32 = AtomicU32::new(1);

fn connect_replies() -> &'static Mutex<HashMap<u32, oneshot::Sender<bool>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<u32, oneshot::Sender<bool>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn handshake_replies() -> &'static Mutex<HashMap<u32, oneshot::Sender<CoreAfterHandshakeDecision>>>
{
    static INSTANCE: OnceLock<Mutex<HashMap<u32, oneshot::Sender<CoreAfterHandshakeDecision>>>> =
        OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[napi]
impl NodeHookReceiver {
    /// Check if the receiver is available.
    #[napi]
    pub fn is_available(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.is_some()
    }

    /// Wait for the next before_connect hook event.
    /// Returns null if the receiver is closed.
    #[napi]
    pub async fn recv_before_connect(&self) -> Result<Option<HookConnectEvent>> {
        // Take receiver temporarily
        let rx = {
            let mut guard = self.inner.lock().unwrap();
            guard.take()
        };
        let Some(mut receiver) = rx else {
            return Ok(None);
        };
        let result = receiver.before_connect_rx.recv().await;
        // Put receiver back
        {
            let mut guard = self.inner.lock().unwrap();
            *guard = Some(receiver);
        }
        match result {
            Some((info, reply_tx)) => {
                let event_id = NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed);
                connect_replies().lock().unwrap().insert(event_id, reply_tx);
                Ok(Some(HookConnectEvent {
                    info: HookConnectInfo {
                        remote_endpoint_id: info.remote_endpoint_id,
                        alpn: info.alpn,
                    },
                    event_id,
                }))
            }
            None => Ok(None),
        }
    }

    /// Respond to a before_connect event (allow or deny).
    #[napi]
    pub fn respond_connect(&self, event_id: u32, allow: bool) -> Result<()> {
        let tx = connect_replies()
            .lock()
            .unwrap()
            .remove(&event_id)
            .ok_or_else(|| {
                napi::Error::from_reason(format!("no pending connect event {event_id}"))
            })?;
        let _ = tx.send(allow);
        Ok(())
    }

    /// Wait for the next after_handshake hook event.
    #[napi]
    pub async fn recv_after_handshake(&self) -> Result<Option<HookHandshakeEvent>> {
        let rx = {
            let mut guard = self.inner.lock().unwrap();
            guard.take()
        };
        let Some(mut receiver) = rx else {
            return Ok(None);
        };
        let result = receiver.after_handshake_rx.recv().await;
        {
            let mut guard = self.inner.lock().unwrap();
            *guard = Some(receiver);
        }
        match result {
            Some((info, reply_tx)) => {
                let event_id = NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed);
                handshake_replies()
                    .lock()
                    .unwrap()
                    .insert(event_id, reply_tx);
                Ok(Some(HookHandshakeEvent {
                    info: HookHandshakeInfo {
                        remote_endpoint_id: info.remote_endpoint_id,
                        alpn: info.alpn,
                        is_alive: info.is_alive,
                    },
                    event_id,
                }))
            }
            None => Ok(None),
        }
    }

    /// Respond to an after_handshake event (accept or reject).
    #[napi]
    pub fn respond_handshake(
        &self,
        event_id: u32,
        accept: bool,
        error_code: Option<u32>,
        reason: Option<String>,
    ) -> Result<()> {
        let tx = handshake_replies()
            .lock()
            .unwrap()
            .remove(&event_id)
            .ok_or_else(|| {
                napi::Error::from_reason(format!("no pending handshake event {event_id}"))
            })?;
        let decision = if accept {
            CoreAfterHandshakeDecision::Accept
        } else {
            CoreAfterHandshakeDecision::Reject {
                error_code: error_code.unwrap_or(0),
                reason: reason.unwrap_or_default().into_bytes(),
            }
        };
        let _ = tx.send(decision);
        Ok(())
    }
}
