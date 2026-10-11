//! Per-call RPC authentication through bounded foreign upcalls.
//!
//! A callback must return an explicit nonempty principal. Rejection, overload,
//! missing handlers, malformed answers, timeout and shutdown never authorize a
//! call, even when the upcall policy is configured fail-open. Credentials and
//! resolved attributes are never cached by this adapter.

use std::sync::Arc;

use anyhow::{ensure, Result};
use async_trait::async_trait;
use tokio::sync::Semaphore;

use super::codec::{self, RpcAuthResponse};
use super::{UpcallError, UpcallKind, Upcalls};
use crate::rpc::{AuthContext, AuthOutcome, Authenticator, RpcStatus, StatusCode};

/// Resource limits shared by every clone of a [`ForeignAuthenticator`].
#[derive(Clone, Copy, Debug)]
pub struct ForeignAuthenticatorOptions {
    /// Concurrent callbacks waiting for an answer; saturation refuses immediately.
    pub max_pending: usize,
    /// Maximum encoded bytes in either a request or a response.
    pub max_payload_bytes: usize,
}

impl Default for ForeignAuthenticatorOptions {
    fn default() -> Self {
        Self {
            max_pending: 256,
            max_payload_bytes: 64 * 1024,
        }
    }
}

/// A foreign [`Authenticator`] using [`UpcallKind::RpcAuthenticate`].
///
/// The queue bound alone is insufficient: a binding can drain requests without
/// answering. A separate semaphore bounds all pending authentications before
/// request sizing/encoding, and is released on completion, timeout or future drop.
/// The kind's [`super::UpcallPolicy::timeout`] bounds each callback (default 5s).
#[derive(Clone, Debug)]
pub struct ForeignAuthenticator {
    upcalls: Upcalls,
    capacity: Arc<Semaphore>,
    max_payload_bytes: usize,
}

impl ForeignAuthenticator {
    pub fn new(upcalls: Upcalls) -> Self {
        let options = ForeignAuthenticatorOptions::default();
        Self {
            upcalls,
            capacity: Arc::new(Semaphore::new(options.max_pending)),
            max_payload_bytes: options.max_payload_bytes,
        }
    }

    pub fn with_options(upcalls: Upcalls, options: ForeignAuthenticatorOptions) -> Result<Self> {
        ensure!(
            options.max_pending > 0 && options.max_pending <= Semaphore::MAX_PERMITS,
            "authentication max_pending must be between 1 and Semaphore::MAX_PERMITS"
        );
        ensure!(
            options.max_payload_bytes > 0,
            "authentication max_payload_bytes must be positive"
        );
        Ok(Self {
            upcalls,
            capacity: Arc::new(Semaphore::new(options.max_pending)),
            max_payload_bytes: options.max_payload_bytes,
        })
    }
}

fn unavailable(error: UpcallError) -> RpcStatus {
    let (code, message) = match error {
        UpcallError::Overloaded => (
            StatusCode::ResourceExhausted,
            "authentication capacity exhausted",
        ),
        UpcallError::Unimplemented(_) => {
            (StatusCode::Unauthenticated, "authentication unavailable")
        }
        _ => (StatusCode::Unavailable, "authentication unavailable"),
    };
    // Do not reflect foreign error text: it may contain credentials.
    RpcStatus::error(code, message)
}

#[async_trait]
impl Authenticator for ForeignAuthenticator {
    async fn authenticate(&self, ctx: &AuthContext) -> Result<AuthOutcome, RpcStatus> {
        let _permit = self.capacity.try_acquire().map_err(|_| {
            RpcStatus::error(
                StatusCode::ResourceExhausted,
                "authentication capacity exhausted",
            )
        })?;
        let size = codec::rpc_auth_request_size(
            &ctx.peer_id,
            &ctx.service,
            &ctx.method,
            ctx.session_id,
            &ctx.metadata,
        );
        if size.is_none_or(|size| size > self.max_payload_bytes) {
            return Err(RpcStatus::error(
                StatusCode::ResourceExhausted,
                "authentication context too large",
            ));
        }
        let request = codec::encode_rpc_auth_request(
            &ctx.peer_id,
            &ctx.service,
            &ctx.method,
            ctx.session_id,
            &ctx.metadata,
        );
        let response = self
            .upcalls
            .call(UpcallKind::RpcAuthenticate, request)
            .await
            .map_err(unavailable)?;
        if response.len() > self.max_payload_bytes {
            return Err(RpcStatus::error(
                StatusCode::Unauthenticated,
                "invalid authentication response",
            ));
        }
        match codec::decode_rpc_auth_response(&response) {
            Ok(RpcAuthResponse::Authenticated {
                principal,
                attributes,
            }) => Ok(AuthOutcome {
                principal: Some(principal),
                attributes,
            }),
            Ok(RpcAuthResponse::Rejected) => Err(RpcStatus::error(
                StatusCode::Unauthenticated,
                "authentication rejected",
            )),
            Err(_) => Err(RpcStatus::error(
                StatusCode::Unauthenticated,
                "invalid authentication response",
            )),
        }
    }
}
