//! Hooks module — connection-level gating (before/after connect).
//!
//! The hook system uses channels from CoreHookReceiver. The JS side
//! receives hook invocations and must respond (allow/deny).

use napi::bindgen_prelude::*;
use napi_derive::napi;

use aster_transport_core::CoreHookReceiver;

/// Hook receiver for connection-level gating.
/// Call `takeHookReceiver()` on IrohNode to get this.
#[napi]
pub struct NodeHookReceiver {
    pub(crate) inner: Option<CoreHookReceiver>,
}

impl NodeHookReceiver {
    pub fn from_core(inner: CoreHookReceiver) -> Self {
        Self { inner: Some(inner) }
    }
}

#[napi]
impl NodeHookReceiver {
    /// Check if the receiver is available.
    #[napi]
    pub fn is_available(&self) -> bool {
        self.inner.is_some()
    }
}
