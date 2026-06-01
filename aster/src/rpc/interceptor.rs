//! Client-side call interceptors + policies (deadline / retry / circuit-breaker).
//!
//! A [`Pipeline`] is attached to an [`RpcConnection`](super::client::RpcConnection)
//! via its builder methods and wraps **unary** calls: it runs the
//! [`Interceptor`] chain (`on_request` → invoke → `on_response`, `on_error`
//! reversed), bounds each attempt by the deadline, retries retryable failures
//! per the [`RetryPolicy`], and fails fast when the [`CircuitBreaker`] is open.
//! Streaming calls are not yet intercepted (a follow-up).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::status::StatusCode;
use crate::error::{Error, Result};

/// Per-call context handed to [`Interceptor`] hooks.
#[derive(Clone, Debug)]
pub struct CallContext {
    pub service: String,
    pub method: String,
    /// 1-based attempt number (incremented across retries).
    pub attempt: u32,
    /// Per-call deadline, if any.
    pub deadline: Option<Duration>,
    /// Request metadata (key/value), as carried in the `StreamHeader`.
    pub metadata: Vec<(String, String)>,
}

/// A client interceptor. Default hooks are pass-through; override what you need.
#[async_trait::async_trait]
pub trait Interceptor: Send + Sync + 'static {
    /// Transform/observe the request payload before it is sent.
    async fn on_request(&self, _ctx: &CallContext, request: Vec<u8>) -> Result<Vec<u8>> {
        Ok(request)
    }
    /// Transform/observe the response payload after a successful call.
    async fn on_response(&self, _ctx: &CallContext, response: Vec<u8>) -> Result<Vec<u8>> {
        Ok(response)
    }
    /// Observe/map an error before it is returned (or retried).
    async fn on_error(&self, _ctx: &CallContext, error: Error) -> Error {
        error
    }
}

/// Retry policy for unary calls. Retries are attempted only when the failure's
/// [`StatusCode`] is in [`retryable`](RetryPolicy::retryable). Enable this only
/// on connections whose calls are safe to retry (idempotent).
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Total attempts (>= 1). `1` disables retries.
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub multiplier: f64,
    /// Status codes that trigger a retry.
    pub retryable: Vec<StatusCode>,
}

impl RetryPolicy {
    /// `max_attempts` total tries with sensible exponential-backoff defaults
    /// (50 ms → ×2 → 5 s cap) and the conventional retryable set
    /// (`Unavailable`, `Aborted`, `ResourceExhausted`).
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_secs(5),
            multiplier: 2.0,
            retryable: vec![
                StatusCode::Unavailable,
                StatusCode::Aborted,
                StatusCode::ResourceExhausted,
            ],
        }
    }

    fn is_retryable(&self, error: &Error) -> bool {
        match error {
            Error::Rpc { code, .. } => self.retryable.iter().any(|s| s.as_i32() == *code),
            _ => false,
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let scaled = self.initial_backoff.as_secs_f64()
            * self.multiplier.powi(attempt.saturating_sub(1) as i32);
        Duration::from_secs_f64(scaled.min(self.max_backoff.as_secs_f64()))
    }
}

/// A simple count-based circuit breaker, shared across the calls on a
/// connection. After `failure_threshold` consecutive failures it opens and
/// fails calls fast with `UNAVAILABLE`; after `recovery` it half-opens to let a
/// single probe through.
#[derive(Clone)]
pub struct CircuitBreaker {
    inner: Arc<Mutex<CbState>>,
}

struct CbState {
    failures: u32,
    threshold: u32,
    recovery: Duration,
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, recovery: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CbState {
                failures: 0,
                threshold: failure_threshold.max(1),
                recovery,
                opened_at: None,
            })),
        }
    }

    /// Whether a call may proceed (CLOSED, or OPEN past its recovery window).
    fn allow(&self) -> bool {
        let mut s = self.inner.lock().unwrap();
        match s.opened_at {
            None => true,
            Some(opened) => {
                if opened.elapsed() >= s.recovery {
                    // Half-open: allow a single probe; reset the timer so further
                    // calls stay blocked until the probe resolves.
                    s.opened_at = Some(Instant::now());
                    true
                } else {
                    false
                }
            }
        }
    }

    fn record_success(&self) {
        let mut s = self.inner.lock().unwrap();
        s.failures = 0;
        s.opened_at = None;
    }

    fn record_failure(&self) {
        let mut s = self.inner.lock().unwrap();
        s.failures += 1;
        if s.failures >= s.threshold {
            s.opened_at = Some(Instant::now());
        }
    }
}

/// The configured client pipeline for a connection. Cheap to clone (shared
/// interceptors + circuit-breaker state).
#[derive(Clone, Default)]
pub struct Pipeline {
    pub(crate) interceptors: Vec<Arc<dyn Interceptor>>,
    pub(crate) retry: Option<RetryPolicy>,
    pub(crate) circuit_breaker: Option<CircuitBreaker>,
    pub(crate) deadline: Option<Duration>,
}

impl Pipeline {
    pub(crate) fn is_noop(&self) -> bool {
        self.interceptors.is_empty()
            && self.retry.is_none()
            && self.circuit_breaker.is_none()
            && self.deadline.is_none()
    }

    /// Run a unary call through the pipeline. `invoke` performs one transport
    /// round-trip for the (interceptor-transformed) request bytes. Each attempt
    /// re-runs `on_request` on the *original* request with the current
    /// `ctx.attempt`, applies the deadline, then runs `on_response`; any failure
    /// in that sequence (request/invoke/deadline/response) is routed through
    /// `on_error` and considered for retry, and updates the circuit breaker.
    pub(crate) async fn run_unary<F, Fut>(
        &self,
        mut ctx: CallContext,
        request: Vec<u8>,
        invoke: F,
    ) -> Result<Vec<u8>>
    where
        F: Fn(Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>>>,
    {
        let mut attempt = 1u32;
        loop {
            ctx.attempt = attempt;

            // Circuit breaker: fail fast (no retry) while open.
            if let Some(cb) = &self.circuit_breaker {
                if !cb.allow() {
                    let err = Error::Rpc {
                        code: StatusCode::Unavailable.as_i32(),
                        message: "circuit breaker open".into(),
                        details: Vec::new(),
                    };
                    return Err(self.apply_on_error(&ctx, err).await);
                }
            }

            match self.attempt_once(&ctx, request.clone(), &invoke).await {
                Ok(resp) => {
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_success();
                    }
                    return Ok(resp);
                }
                Err(err) => {
                    if let Some(cb) = &self.circuit_breaker {
                        cb.record_failure();
                    }
                    let err = self.apply_on_error(&ctx, err).await;
                    if let Some(retry) = &self.retry {
                        if attempt < retry.max_attempts && retry.is_retryable(&err) {
                            tokio::time::sleep(retry.backoff(attempt)).await;
                            attempt += 1;
                            continue;
                        }
                    }
                    return Err(err);
                }
            }
        }
    }

    /// One attempt: `on_request` (from the original bytes) → invoke (deadline) →
    /// `on_response`. Any step's error short-circuits the attempt.
    async fn attempt_once<F, Fut>(
        &self,
        ctx: &CallContext,
        original: Vec<u8>,
        invoke: &F,
    ) -> Result<Vec<u8>>
    where
        F: Fn(Vec<u8>) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<u8>>>,
    {
        let mut req = original;
        for i in &self.interceptors {
            req = i.on_request(ctx, req).await?;
        }

        let fut = invoke(req);
        let resp = match self.deadline {
            Some(d) => match tokio::time::timeout(d, fut).await {
                Ok(r) => r?,
                Err(_) => {
                    return Err(Error::Rpc {
                        code: StatusCode::DeadlineExceeded.as_i32(),
                        message: format!("deadline exceeded after {d:?}"),
                        details: Vec::new(),
                    })
                }
            },
            None => fut.await?,
        };

        let mut resp = resp;
        for i in &self.interceptors {
            resp = i.on_response(ctx, resp).await?;
        }
        Ok(resp)
    }

    async fn apply_on_error(&self, ctx: &CallContext, mut error: Error) -> Error {
        for i in self.interceptors.iter().rev() {
            error = i.on_error(ctx, error).await;
        }
        error
    }
}
