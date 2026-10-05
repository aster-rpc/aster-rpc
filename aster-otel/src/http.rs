//! The OTLP/HTTP client: blocking reqwest on rustls (ring provider, platform
//! verifier), wrapped so the request headers are exactly the resolved ones.
//!
//! Why the wrapper: `opentelemetry-otlp` 0.33 merges `OTEL_EXPORTER_OTLP_HEADERS`
//! (and the per-signal variable) from the process environment *over* the
//! headers passed to its builder (`exporter/http/mod.rs`, `build_client`). That
//! would let env beat a programmatic header, and would apply env headers even
//! with `from_env = false`. The wrapper drops every header the exporter set
//! except its own protocol headers, then sets the resolved ones.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::header::{
    HeaderName, HeaderValue, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, USER_AGENT,
};
use opentelemetry_http::{HttpClient, HttpError, Request, Response};

use crate::config::OtlpHeaders;
use crate::error::{Error, Result};

/// Build the blocking reqwest client. reqwest's blocking client panics if it is
/// built inside a Tokio runtime (debug builds), and bindings call `init` from
/// runtime threads, so it is built on a short-lived thread of its own (as
/// `opentelemetry-otlp` does for its default client).
pub(crate) fn blocking_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    std::thread::Builder::new()
        .name("aster-otel-http-init".into())
        .spawn(
            move || -> std::result::Result<reqwest::blocking::Client, String> {
                let provider = Arc::new(rustls::crypto::ring::default_provider());
                let tls = rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|e| format!("TLS protocol versions: {e}"))?;
                let tls = rustls_platform_verifier::BuilderVerifierExt::with_platform_verifier(tls)
                    .map_err(|e| format!("platform certificate verifier: {e}"))?
                    .with_no_client_auth();
                reqwest::blocking::Client::builder()
                    .timeout(timeout)
                    .tls_backend_preconfigured(tls)
                    .build()
                    .map_err(|e| format!("HTTP client: {e}"))
            },
        )
        .map_err(|e| Error::Exporter(format!("could not spawn the HTTP client thread: {e}")))?
        .join()
        .map_err(|_| Error::Exporter("the HTTP client thread panicked".into()))?
        .map_err(Error::Exporter)
}

/// An [`HttpClient`] that pins the request headers to the resolved set.
pub(crate) struct PinnedHeaders {
    inner: reqwest::blocking::Client,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl PinnedHeaders {
    pub(crate) fn new(inner: reqwest::blocking::Client, headers: &OtlpHeaders) -> Result<Self> {
        let mut out = Vec::with_capacity(headers.len());
        for (i, (k, v)) in headers.iter().enumerate() {
            let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| {
                Error::Config(format!("OTLP header #{} has an invalid name", i + 1))
            })?;
            let mut value = HeaderValue::from_str(v).map_err(|_| {
                Error::Config(format!("OTLP header #{} has an invalid value", i + 1))
            })?;
            value.set_sensitive(true);
            out.push((name, value));
        }
        Ok(PinnedHeaders {
            inner,
            headers: out,
        })
    }
}

impl fmt::Debug for PinnedHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedHeaders")
            .field("headers", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl HttpClient for PinnedHeaders {
    async fn send_bytes(
        &self,
        mut request: Request<Bytes>,
    ) -> std::result::Result<Response<Bytes>, HttpError> {
        let h = request.headers_mut();
        let keep = [CONTENT_TYPE, CONTENT_ENCODING, CONTENT_LENGTH, USER_AGENT];
        let kept: Vec<(HeaderName, HeaderValue)> = keep
            .iter()
            .filter_map(|k| h.get(k).map(|v| (k.clone(), v.clone())))
            .collect();
        h.clear();
        for (k, v) in kept.into_iter().chain(self.headers.iter().cloned()) {
            h.insert(k, v);
        }
        self.inner.send_bytes(request).await
    }
}
