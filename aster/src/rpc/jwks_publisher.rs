//! The issuer-key publisher (gateway identity, slice 3; design: "Issuer keys"
//! in `docs/_internal/web_gateway/gateway-identity.md`).
//!
//! On the node configured with `[identity.publisher]` (normally the gateway,
//! which already talks HTTPS to the identity providers), the publisher fetches
//! each issuer's JWK set and writes it into the fleet namespace with
//! [`publish_issuer_keys`](super::issuer_keys::publish_issuer_keys), where
//! every core reads it. Cores never fetch anything.
//!
//! ## Fetching
//!
//! - HTTPS only, TLS validated against the built-in WebPKI roots plus
//!   `ca_certs` (or only `ca_certs` with `ca_only`), as for `relay_ca_certs`.
//! - No redirects, no proxy, no decompression; connect within
//!   [`CONNECT_TIMEOUT`], the whole fetch within [`FETCH_TIMEOUT`], at most
//!   [`MAX_JWKS_JSON_BYTES`] of body, status `200` only.
//! - The body must be a JWK set (`{"keys": [...]}`). Keys of an unsupported
//!   type or algorithm, private keys and non-signature keys are dropped with
//!   a warning (counted, never logged in full); a set with no usable key is
//!   a failure.
//!
//! ## Refreshing
//!
//! Each issuer is refreshed every `refresh_secs` (±10 % jitter). A failure
//! (fetch, validation, write grant, write) is retried with exponential
//! backoff from [`BACKOFF_BASE`] up to `refresh_secs` (with jitter), and
//! **never** replaces the last good record: only a fetched, valid set is
//! written. Every successful refresh writes the record again with a new
//! `fetched_at`, which keeps it inside the cores' `max_key_set_age_secs`.
//!
//! A token naming a `kid` the publisher has not published for its issuer
//! (usually a rotation at the provider) refreshes that issuer at once,
//! through [`PublisherHandle::notice_kid`]: `AsterClient` calls it for every
//! call it makes with `aster-on-behalf-of`, and an `AsterServer` for every
//! provider token it refuses for an unknown `kid`. Such refreshes run at most
//! every `min_refresh_secs` per issuer; a notice inside that gap is served
//! when it ends.
//!
//! ## Writing
//!
//! Only the root can write the fleet namespace. Before each write the
//! publisher opens the root's sealed grant of the namespace's write
//! capability to this node ([`open_publisher_grant`]); without an active
//! grant it writes nothing. The record carries this node's enrollment
//! credential, which must grant `aster.identity-publisher` for cores to
//! accept it. After a write the publisher synchronizes the fleet namespace
//! with its delivery peers (the server an `AsterClient` is connected to, an
//! `AsterServer`'s `[enrollment] replicas`). On a root rotation the next
//! attempt opens the new root's fleet namespace and grant.
//!
//! [`open_publisher_grant`]: crate::enrollment::open_publisher_grant

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::config::IdentityPublisherConfig;
use crate::enrollment::{Enrollment, PublisherGrant};
use crate::log_targets::IDENTITY;
use crate::{Error, Node, NodeId, Result};

pub use super::issuer_keys::MAX_JWKS_JSON_BYTES;
use super::issuer_keys::{publish_issuer_keys, IssuerKeyPublication};

/// Bound on connecting to an identity provider.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on one whole JWK-set fetch.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// First retry delay after a failure; doubles per failure up to
/// `refresh_secs`.
pub const BACKOFF_BASE: Duration = Duration::from_secs(5);
/// Bound on synchronizing the fleet namespace with one delivery peer.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);
/// Most unknown-`kid` notices queued; more are dropped (the queued ones
/// already force a refresh).
const NOTICE_QUEUE: usize = 256;

// ── Status ──────────────────────────────────────────────────────────────────

/// Why a refresh of one issuer did not publish. The last good record stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// Connecting, TLS, or reading the response failed (TLS validation
    /// failures included), or it timed out.
    Fetch,
    /// The provider answered with this status (only `200` is accepted;
    /// redirects are not followed).
    Status(u16),
    /// The body is larger than [`MAX_JWKS_JSON_BYTES`].
    TooLarge,
    /// The body is not a JWK set, or the record it makes is invalid.
    Invalid,
    /// The JWK set holds no usable signature key.
    NoUsableKeys,
    /// This node holds no active write grant for the fleet namespace yet.
    NoWriteGrant,
    /// The root withdrew this node's write grant.
    GrantWithdrawn,
    /// The record could not be written.
    Write,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch => write!(f, "fetching the JWK set failed"),
            Self::Status(code) => write!(f, "the JWK set URL answered {code}"),
            Self::TooLarge => write!(f, "the JWK set is larger than {MAX_JWKS_JSON_BYTES} bytes"),
            Self::Invalid => write!(f, "the response is not a valid JWK set"),
            Self::NoUsableKeys => write!(f, "the JWK set has no usable signature key"),
            Self::NoWriteGrant => write!(f, "no write grant for the fleet namespace"),
            Self::GrantWithdrawn => write!(f, "the fleet namespace write grant was withdrawn"),
            Self::Write => write!(f, "writing the key set failed"),
        }
    }
}

impl std::error::Error for PublishError {}

/// What the publisher last did for one issuer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssuerPublishStatus {
    /// The key ids of the last published set, in order; empty before the
    /// first publication.
    pub kids: Vec<String>,
    /// `fetched_at` of the last published set.
    pub fetched_at: Option<u64>,
    /// Sets published so far.
    pub publications: u64,
    /// Refresh attempts so far (published or not).
    pub attempts: u64,
    /// Of those, attempts an unknown-`kid` notice brought forward from the
    /// schedule.
    pub forced_attempts: u64,
    /// Why the latest attempt did not publish; `None` after a success.
    pub last_error: Option<PublishError>,
}

/// One published set, as the publisher's [`Backend`] hands it on.
#[derive(Debug, Clone)]
pub(crate) struct Fetched {
    pub(crate) jwks_json: String,
    pub(crate) kids: Vec<String>,
    pub(crate) fetched_at: u64,
}

struct Shared {
    issuers: BTreeMap<String, String>,
    status: Mutex<BTreeMap<String, IssuerPublishStatus>>,
    generation: watch::Sender<u64>,
    notices: mpsc::Sender<String>,
}

/// A handle to a running publisher: unknown-`kid` notices and its status.
/// Cheap to clone.
#[derive(Clone)]
pub struct PublisherHandle(Arc<Shared>);

impl std::fmt::Debug for PublisherHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublisherHandle")
            .field("issuers", &self.0.issuers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl PublisherHandle {
    /// A token of `iss` names `kid`. When `iss` is one this node publishes and
    /// `kid` is not in the set it last published, refresh `iss` now (at most
    /// every `min_refresh_secs`). Never blocks.
    pub fn notice_kid(&self, iss: &str, kid: &str) {
        if !self.0.issuers.contains_key(iss) {
            return;
        }
        let known = self
            .0
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(iss)
            .is_some_and(|status| status.kids.iter().any(|k| k == kid));
        if !known {
            // A full queue already forces refreshes.
            let _ = self.0.notices.try_send(iss.to_string());
        }
    }

    /// [`Self::notice_kid`] for a compact JWT's unverified `iss` and `kid`;
    /// anything that is not a provider token is ignored.
    pub fn notice_token(&self, token: &str) {
        use aster_transport_core::delegation::provider_token::peek_issuer_and_kid;
        if let Ok((iss, kid)) = peek_issuer_and_kid(token) {
            self.notice_kid(&iss, &kid);
        }
    }

    /// Each configured issuer's status.
    pub fn status(&self) -> BTreeMap<String, IssuerPublishStatus> {
        self.0
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `iss`'s status.
    pub fn issuer_status(&self, iss: &str) -> Option<IssuerPublishStatus> {
        self.status().remove(iss)
    }

    /// A receiver that changes after every attempt, for condition waits:
    /// subscribe, check, then `changed().await` and check again.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.generation.subscribe()
    }

    fn record(
        &self,
        iss: &str,
        forced: bool,
        outcome: std::result::Result<&Fetched, &PublishError>,
    ) {
        {
            let mut status = self.0.status.lock().unwrap_or_else(|e| e.into_inner());
            let entry = status.entry(iss.to_string()).or_default();
            entry.attempts += 1;
            entry.forced_attempts += u64::from(forced);
            match outcome {
                Ok(fetched) => {
                    entry.kids = fetched.kids.clone();
                    entry.fetched_at = Some(fetched.fetched_at);
                    entry.publications += 1;
                    entry.last_error = None;
                }
                Err(error) => entry.last_error = Some(error.clone()),
            }
        }
        self.0.generation.send_modify(|generation| *generation += 1);
    }
}

// ── Scheduling ──────────────────────────────────────────────────────────────

/// One issuer's refresh schedule. Pure: the caller supplies the time and the
/// jitter (uniform in `[0, 1)`).
#[derive(Debug, Clone)]
pub(crate) struct Schedule {
    refresh: Duration,
    min_refresh: Duration,
    failures: u32,
    next: Instant,
    /// When the last unknown-`kid` refresh was due.
    last_forced: Option<Instant>,
}

impl Schedule {
    /// Due now: the first fetch runs at start.
    pub(crate) fn new(now: Instant, refresh: Duration, min_refresh: Duration) -> Self {
        Self {
            refresh,
            min_refresh,
            failures: 0,
            next: now,
            last_forced: None,
        }
    }

    pub(crate) fn next(&self) -> Instant {
        self.next
    }

    /// Whether the refresh now due was brought forward by a notice.
    pub(crate) fn forced(&self) -> bool {
        self.last_forced == Some(self.next)
    }

    /// After a published refresh: the next is `refresh` away, ±10 %.
    pub(crate) fn succeeded(&mut self, now: Instant, jitter: f64) {
        self.failures = 0;
        self.next = now + self.refresh.mul_f64(0.9 + 0.2 * jitter);
    }

    /// After a failed refresh: back off exponentially from
    /// [`BACKOFF_BASE`] to `refresh`, each delay between half and all of its
    /// step.
    pub(crate) fn failed(&mut self, now: Instant, jitter: f64) {
        self.failures = self.failures.saturating_add(1);
        let step = BACKOFF_BASE
            .saturating_mul(1u32 << (self.failures - 1).min(20))
            .min(self.refresh);
        self.next = now + step.mul_f64(0.5 + 0.5 * jitter);
    }

    /// An unknown `kid` was seen: refresh now, or when `min_refresh` since the
    /// last such refresh has passed, whichever is later (never later than
    /// already due).
    pub(crate) fn notice(&mut self, now: Instant) {
        let due = match self.last_forced {
            Some(last) => now.max(last + self.min_refresh),
            None => now,
        };
        if due < self.next {
            self.next = due;
            self.last_forced = Some(due);
        }
    }
}

fn jitter() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::getrandom(&mut bytes).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

// ── The work, behind a seam for the scheduling tests ────────────────────────

#[async_trait::async_trait]
pub(crate) trait Backend: Send + Sync + 'static {
    /// Fetch and validate `iss`'s JWK set from `url`.
    async fn fetch(&self, iss: &str, url: &str) -> std::result::Result<Fetched, PublishError>;
    /// Write `fetched` as `iss`'s record and deliver it.
    async fn publish(
        &self,
        iss: &str,
        url: &str,
        fetched: &Fetched,
    ) -> std::result::Result<(), PublishError>;
}

async fn run(
    handle: PublisherHandle,
    backend: Arc<dyn Backend>,
    refresh: Duration,
    min_refresh: Duration,
    mut notices: mpsc::Receiver<String>,
) {
    let now = Instant::now();
    let mut schedules: BTreeMap<String, Schedule> = handle
        .0
        .issuers
        .keys()
        .map(|iss| (iss.clone(), Schedule::new(now, refresh, min_refresh)))
        .collect();
    loop {
        let now = Instant::now();
        let due: Vec<String> = schedules
            .iter()
            .filter(|(_, schedule)| schedule.next() <= now)
            .map(|(iss, _)| iss.clone())
            .collect();
        for iss in due {
            let url = &handle.0.issuers[&iss];
            let forced = schedules[&iss].forced();
            let outcome = match backend.fetch(&iss, url).await {
                Ok(fetched) => backend.publish(&iss, url, &fetched).await.map(|()| fetched),
                Err(error) => Err(error),
            };
            let schedule = schedules.get_mut(&iss).expect("scheduled issuer");
            match &outcome {
                Ok(fetched) => {
                    tracing::info!(
                        target: IDENTITY,
                        iss = iss.as_str(),
                        keys = fetched.kids.len(),
                        fetched_at = fetched.fetched_at,
                        "issuer keys published"
                    );
                    schedule.succeeded(Instant::now(), jitter());
                }
                Err(error) => {
                    tracing::warn!(
                        target: IDENTITY,
                        iss = iss.as_str(),
                        error = %error,
                        "issuer keys not published; the last published set stays"
                    );
                    schedule.failed(Instant::now(), jitter());
                }
            }
            handle.record(&iss, forced, outcome.as_ref());
        }
        let next = schedules
            .values()
            .map(Schedule::next)
            .min()
            .unwrap_or_else(|| Instant::now() + refresh);
        tokio::select! {
            () = tokio::time::sleep_until(next) => {}
            notice = notices.recv() => match notice {
                Some(iss) => {
                    if let Some(schedule) = schedules.get_mut(&iss) {
                        schedule.notice(Instant::now());
                    }
                }
                None => break,
            },
        }
    }
}

// ── The publisher ───────────────────────────────────────────────────────────

/// A running publisher; dropping it stops refreshing (published records stay
/// until they age out at the cores).
pub struct IssuerKeyPublisher {
    handle: PublisherHandle,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for IssuerKeyPublisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::fmt::Debug for IssuerKeyPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.handle.fmt(f)
    }
}

impl IssuerKeyPublisher {
    /// Start publishing for `config` (validated against
    /// `max_key_set_age_secs`) on `node`, which runs `enrollment`, delivering
    /// every written record to `deliver_to`. `AsterServer` and `AsterClient`
    /// start one from `[identity.publisher]`; this is the escape hatch.
    pub fn start(
        node: &Node,
        enrollment: Enrollment,
        config: &IdentityPublisherConfig,
        max_key_set_age_secs: u64,
        deliver_to: Vec<NodeId>,
    ) -> Result<Self> {
        config.validate(max_key_set_age_secs)?;
        let issuers = issuer_urls(config)?;
        let client = http_client(config)?;
        let backend = Arc::new(Live {
            node: node.clone(),
            enrollment,
            client,
            deliver_to,
        });
        Ok(Self::start_with(
            issuers,
            backend,
            Duration::from_secs(config.refresh_secs),
            Duration::from_secs(config.min_refresh_secs),
        ))
    }

    pub(crate) fn start_with(
        issuers: BTreeMap<String, String>,
        backend: Arc<dyn Backend>,
        refresh: Duration,
        min_refresh: Duration,
    ) -> Self {
        let (notices, receiver) = mpsc::channel(NOTICE_QUEUE);
        let handle = PublisherHandle(Arc::new(Shared {
            status: Mutex::new(
                issuers
                    .keys()
                    .map(|iss| (iss.clone(), IssuerPublishStatus::default()))
                    .collect(),
            ),
            issuers,
            generation: watch::Sender::new(0),
            notices,
        }));
        let task = tokio::spawn(run(handle.clone(), backend, refresh, min_refresh, receiver));
        Self { handle, task }
    }

    /// The handle for notices and status.
    pub fn handle(&self) -> PublisherHandle {
        self.handle.clone()
    }
}

/// Each issuer's parsed URL, after the checks a URL parser can make: an
/// `https` URL with a host and no user info or fragment.
fn issuer_urls(config: &IdentityPublisherConfig) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for iss in &config.issuers {
        let raw = &config.jwks_url[iss];
        let url = reqwest::Url::parse(raw).map_err(|e| {
            Error::InvalidArgument(format!(
                "[identity.publisher] jwks_url of {iss:?} is not a URL: {e}"
            ))
        })?;
        if url.scheme() != "https"
            || url.host_str().is_none_or(str::is_empty)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidArgument(format!(
                "[identity.publisher] jwks_url of {iss:?} must be an https URL with a host and \
                 no user info or fragment"
            )));
        }
        out.insert(iss.clone(), url.to_string());
    }
    Ok(out)
}

/// The HTTPS client: validated TLS (WebPKI roots plus `ca_certs`, or only
/// `ca_certs`), no redirects, no proxy, bounded in time.
fn http_client(config: &IdentityPublisherConfig) -> Result<reqwest::Client> {
    let files: Vec<String> = config
        .ca_certs
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let ca = aster_transport_core::network_config::ca_tls_config(
        &files,
        config.ca_only,
        "identity.publisher.ca_certs",
    )
    .map_err(|e| Error::InvalidArgument(format!("{e:#}")))?
    .unwrap_or_else(iroh::tls::CaTlsConfig::embedded);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = ca
        .server_cert_verifier(provider.clone())
        .map_err(|e| Error::InvalidArgument(format!("identity.publisher TLS roots: {e}")))?;
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::InvalidArgument(format!("identity.publisher TLS: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!(
            "aster/",
            env!("CARGO_PKG_VERSION"),
            " identity-publisher"
        ))
        .tls_backend_preconfigured(tls)
        .build()
        .map_err(|_| Error::InvalidArgument("identity.publisher: the HTTPS client".into()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Validate a fetched body as a JWK set: JSON `{"keys": [...]}` with at least
/// one usable signature key. Unusable keys are dropped with a warning.
pub(crate) fn validate_jwks(
    iss: &str,
    body: &[u8],
    fetched_at: u64,
) -> std::result::Result<Fetched, PublishError> {
    use aster_transport_core::delegation::provider_token::parse_jwk_set;
    if body.len() > MAX_JWKS_JSON_BYTES {
        return Err(PublishError::TooLarge);
    }
    let text = std::str::from_utf8(body).map_err(|_| PublishError::Invalid)?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| PublishError::Invalid)?;
    let served = value
        .get("keys")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .ok_or(PublishError::Invalid)?;
    let keys = parse_jwk_set(&value).map_err(|_| PublishError::Invalid)?;
    if keys.len() < served {
        tracing::warn!(
            target: IDENTITY,
            iss,
            served,
            usable = keys.len(),
            "issuer JWK set: keys dropped (unsupported type or algorithm, private, not for \
             signatures, or a repeated kid)"
        );
    }
    if keys.is_empty() {
        return Err(PublishError::NoUsableKeys);
    }
    Ok(Fetched {
        jwks_json: text.to_string(),
        kids: keys.iter().map(|key| key.kid().to_string()).collect(),
        fetched_at,
    })
}

struct Live {
    node: Node,
    enrollment: Enrollment,
    client: reqwest::Client,
    deliver_to: Vec<NodeId>,
}

impl Live {
    /// The fleet namespace opened for writing, from the root's grant. Pulls
    /// from the delivery peers once when the grant is not there yet.
    async fn writable_fleet(&self) -> std::result::Result<crate::Doc, PublishError> {
        let fleet = self.enrollment.fleet_namespace();
        let root = self.enrollment.trusted_root();
        let secret = self
            .node
            .export_secret_key()
            .map_err(|_| PublishError::NoWriteGrant)?;
        let doc = self
            .node
            .docs()
            .open_or_import_read_namespace(crate::NamespaceId::from_bytes(fleet))
            .await
            .map_err(|_| PublishError::NoWriteGrant)?;
        let mut grant = crate::enrollment::open_publisher_grant(&doc, &root, &secret).await;
        if !matches!(grant, Ok(PublisherGrant::Active(_))) && !self.deliver_to.is_empty() {
            let _ = doc
                .sync_with_timeout(&self.deliver_to, DELIVERY_TIMEOUT)
                .await;
            grant = crate::enrollment::open_publisher_grant(&doc, &root, &secret).await;
        }
        let write = match grant {
            Ok(PublisherGrant::Active(write)) => write,
            Ok(PublisherGrant::Revoked) => return Err(PublishError::GrantWithdrawn),
            Ok(PublisherGrant::Absent) => return Err(PublishError::NoWriteGrant),
            Err(error) => {
                tracing::warn!(target: IDENTITY, error = %error, "publisher grant unusable");
                return Err(PublishError::NoWriteGrant);
            }
        };
        let doc = self
            .node
            .docs()
            .open_or_import_write_namespace(crate::NamespaceSecret::from_bytes(write))
            .await
            .map_err(|_| PublishError::Write)?;
        let id = doc.id().map_err(|_| PublishError::Write)?;
        if id.to_bytes() != fleet {
            return Err(PublishError::NoWriteGrant);
        }
        Ok(doc)
    }
}

#[async_trait::async_trait]
impl Backend for Live {
    async fn fetch(&self, iss: &str, url: &str) -> std::result::Result<Fetched, PublishError> {
        let mut response = self
            .client
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "application/jwk-set+json, application/json",
            )
            .send()
            .await
            .map_err(|_| PublishError::Fetch)?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(PublishError::Status(status));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_JWKS_JSON_BYTES as u64)
        {
            return Err(PublishError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| PublishError::Fetch)? {
            if body.len() + chunk.len() > MAX_JWKS_JSON_BYTES {
                return Err(PublishError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        validate_jwks(iss, &body, unix_now())
    }

    async fn publish(
        &self,
        iss: &str,
        url: &str,
        fetched: &Fetched,
    ) -> std::result::Result<(), PublishError> {
        let fleet = self.writable_fleet().await?;
        let credential = self.enrollment.credential().map(|(chain, _)| chain);
        publish_issuer_keys(
            &self.node,
            &fleet,
            credential.as_deref(),
            &IssuerKeyPublication {
                iss: iss.to_string(),
                source_url: url.to_string(),
                jwks_json: fetched.jwks_json.clone(),
                fetched_at: fetched.fetched_at,
            },
        )
        .await
        .map_err(|error| match error {
            Error::InvalidArgument(_) => PublishError::Invalid,
            _ => PublishError::Write,
        })?;
        for peer in &self.deliver_to {
            if let Err(error) = fleet
                .sync_with_timeout(std::slice::from_ref(peer), DELIVERY_TIMEOUT)
                .await
            {
                // Written locally; live sync and the next refresh retry.
                tracing::debug!(
                    target: IDENTITY,
                    peer = peer.as_str(),
                    error = %error,
                    "issuer keys: delivery did not complete"
                );
            }
        }
        Ok(())
    }
}

// ── Client-side notices ─────────────────────────────────────────────────────

/// An `AsterClient` interceptor that hands every forwarded token's
/// `iss`/`kid` to the node's publisher before the call goes out.
pub(crate) struct NoticeInterceptor(pub(crate) PublisherHandle);

#[async_trait::async_trait]
impl super::Interceptor for NoticeInterceptor {
    async fn on_request(
        &self,
        ctx: &super::ClientCallContext,
        request: Vec<u8>,
    ) -> Result<Vec<u8>> {
        for (key, value) in &ctx.metadata {
            if key.eq_ignore_ascii_case(super::identity::ON_BEHALF_OF_METADATA) {
                self.0.notice_token(value);
            }
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ISS: &str = "https://idp.example.com";
    const URL: &str = "https://idp.example.com/jwks";

    #[test]
    fn schedule_jitters_backs_off_and_rate_limits_notices() {
        let start = Instant::now();
        let refresh = Duration::from_secs(3600);
        let mut s = Schedule::new(start, refresh, Duration::from_secs(60));
        assert_eq!(s.next(), start, "the first fetch runs at once");
        s.succeeded(start, 0.0);
        assert_eq!(s.next(), start + Duration::from_secs(3240));
        s.succeeded(start, 0.999_999);
        assert!(s.next() < start + Duration::from_secs(3961));
        // Backoff: 5 s, 10 s, 20 s, ... capped at refresh; half to all.
        s.failed(start, 0.999_999);
        assert!(s.next() <= start + Duration::from_secs(5));
        s.failed(start, 0.0);
        assert_eq!(s.next(), start + Duration::from_secs(5));
        for _ in 0..20 {
            s.failed(start, 0.999_999);
        }
        assert!(s.next() <= start + refresh);
        assert!(s.next() > start + refresh / 2);
        // Notices: the first is immediate, the next waits out min_refresh.
        let mut s = Schedule::new(start, refresh, Duration::from_secs(60));
        s.succeeded(start, 0.5);
        s.notice(start + Duration::from_secs(10));
        assert_eq!(s.next(), start + Duration::from_secs(10));
        s.succeeded(start + Duration::from_secs(10), 0.5);
        s.notice(start + Duration::from_secs(20));
        assert_eq!(s.next(), start + Duration::from_secs(70));
        // A notice never delays a refresh already due sooner.
        let mut s = Schedule::new(start, refresh, Duration::from_secs(60));
        s.notice(start);
        assert_eq!(s.next(), start);
    }

    #[test]
    fn jwks_validation_drops_unusable_keys_and_fails_closed() {
        let ok = serde_json::json!({"keys": [
            {"kty": "EC", "crv": "P-256", "kid": "ec",
             "x": "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU",
             "y": "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0"},
            {"kty": "oct", "kid": "hmac", "k": "AAAA"},
            {"kty": "OKP", "crv": "Ed25519", "kid": "ed", "x": "AAAA"},
        ]})
        .to_string();
        let fetched = validate_jwks(ISS, ok.as_bytes(), 7).unwrap();
        assert_eq!(fetched.kids, ["ec"]);
        assert_eq!(fetched.fetched_at, 7);
        assert_eq!(
            validate_jwks(
                ISS,
                br#"{"keys": [{"kty": "oct", "kid": "h", "k": "AA"}]}"#,
                7
            )
            .unwrap_err(),
            PublishError::NoUsableKeys
        );
        assert_eq!(
            validate_jwks(ISS, br#"{"keys": []}"#, 7).unwrap_err(),
            PublishError::NoUsableKeys
        );
        for bad in [&b"not json"[..], b"[]", br#"{"keys": {}}"#, b"\xff\xfe"] {
            assert_eq!(
                validate_jwks(ISS, bad, 7).unwrap_err(),
                PublishError::Invalid
            );
        }
        let huge = vec![b' '; MAX_JWKS_JSON_BYTES + 1];
        assert_eq!(
            validate_jwks(ISS, &huge, 7).unwrap_err(),
            PublishError::TooLarge
        );
    }

    #[test]
    fn publisher_config_validates_urls_and_the_age_interplay() {
        let good = IdentityPublisherConfig::new([(ISS, URL)]);
        good.validate(172_800).unwrap();
        issuer_urls(&good).unwrap();
        let refused = |config: IdentityPublisherConfig, max_age: u64| {
            config.validate(max_age).unwrap_err().to_string()
        };
        assert!(refused(
            IdentityPublisherConfig::new([(ISS, "http://idp/jwks")]),
            172_800
        )
        .contains("https"));
        assert!(refused(
            IdentityPublisherConfig::new([(ISS, "https://user:pw@idp/jwks")]),
            172_800
        )
        .contains("user info"));
        assert!(refused(
            IdentityPublisherConfig::new([(ISS, "https:///jwks")]),
            172_800
        )
        .contains("host"));
        assert!(refused(good.clone().refresh_secs(10), 172_800).contains("refresh_secs"));
        assert!(
            refused(good.clone().refresh_secs(7200), 7200).contains("half"),
            "a refresh interval above half the max age is refused"
        );
        assert!(refused(good.clone().min_refresh_secs(0), 172_800).contains("min_refresh"));
        assert!(refused(good.clone().ca_only(true), 172_800).contains("ca_only"));
        let mut missing = good.clone();
        missing.issuers.push("https://other".into());
        assert!(refused(missing, 172_800).contains("no jwks_url"));
        let mut extra = good.clone();
        extra.issuers.clear();
        extra.issuers.push("https://other".into());
        extra.jwks_url.insert("https://other".into(), URL.into());
        assert!(refused(extra, 172_800).contains("not in issuers"));
        assert!(refused(
            IdentityPublisherConfig::new([("a".repeat(64).as_str(), URL)]),
            172_800
        )
        .contains("node id"));
    }

    /// A backend whose fetches follow a script: `Ok` publishes, `Err` fails.
    struct Scripted {
        fetches: AtomicUsize,
        publishes: Mutex<Vec<Vec<String>>>,
        script: Mutex<Vec<std::result::Result<Vec<&'static str>, PublishError>>>,
    }

    #[async_trait::async_trait]
    impl Backend for Scripted {
        async fn fetch(&self, _: &str, _: &str) -> std::result::Result<Fetched, PublishError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let next = {
                let mut script = self.script.lock().unwrap();
                if script.len() > 1 {
                    script.remove(0)
                } else {
                    script[0].clone()
                }
            };
            next.map(|kids| Fetched {
                jwks_json: String::new(),
                kids: kids.into_iter().map(str::to_string).collect(),
                fetched_at: 1,
            })
        }
        async fn publish(
            &self,
            _: &str,
            _: &str,
            fetched: &Fetched,
        ) -> std::result::Result<(), PublishError> {
            self.publishes.lock().unwrap().push(fetched.kids.clone());
            Ok(())
        }
    }

    async fn attempts(handle: &PublisherHandle, n: u64) -> IssuerPublishStatus {
        let mut changes = handle.subscribe();
        loop {
            let status = handle.issuer_status(ISS).unwrap();
            if status.attempts >= n {
                return status;
            }
            changes.changed().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refreshes_on_schedule_backs_off_and_follows_unknown_kids() {
        let backend = Arc::new(Scripted {
            fetches: AtomicUsize::new(0),
            publishes: Mutex::new(Vec::new()),
            script: Mutex::new(vec![
                Ok(vec!["k1"]),
                // The scheduled refresh picks up a rotation.
                Ok(vec!["k2"]),
                // Then the provider fails: nothing is published.
                Err(PublishError::Status(503)),
                Err(PublishError::NoUsableKeys),
                Ok(vec!["k3"]),
            ]),
        });
        let publisher = IssuerKeyPublisher::start_with(
            [(ISS.to_string(), URL.to_string())].into(),
            backend.clone(),
            Duration::from_secs(3600),
            Duration::from_secs(60),
        );
        let handle = publisher.handle();
        let first = attempts(&handle, 1).await;
        assert_eq!(first.kids, ["k1"]);
        let started = Instant::now();
        // The scheduled refresh, about an hour later (time is paused, so this
        // advances only as far as the next timer).
        let second = attempts(&handle, 2).await;
        assert_eq!(second.kids, ["k2"]);
        let waited = Instant::now() - started;
        assert!(
            waited >= Duration::from_secs(3240) && waited <= Duration::from_secs(3960),
            "{waited:?}"
        );
        // An unknown kid refreshes at once; the failure keeps k2 published.
        let before = Instant::now();
        handle.notice_kid(ISS, "k9");
        let third = attempts(&handle, 3).await;
        assert!(Instant::now() - before < Duration::from_secs(1));
        assert_eq!(third.kids, ["k2"]);
        assert_eq!(third.last_error, Some(PublishError::Status(503)));
        assert_eq!(
            (second.forced_attempts, third.forced_attempts),
            (0, 1),
            "the notice's refresh is counted as forced"
        );
        // Backoff retry after 2.5..=5 s, failing again; then 5..=10 s.
        let fourth = attempts(&handle, 4).await;
        assert_eq!(fourth.last_error, Some(PublishError::NoUsableKeys));
        assert_eq!(fourth.kids, ["k2"]);
        let fifth = attempts(&handle, 5).await;
        assert_eq!(fifth.kids, ["k3"]);
        assert_eq!(fifth.last_error, None);
        assert_eq!(fifth.forced_attempts, 1, "backoff retries are not forced");
        assert_eq!(
            *backend.publishes.lock().unwrap(),
            [vec!["k1"], vec!["k2"], vec!["k3"]]
        );
        // A known kid triggers nothing; unknown ones at most once a minute.
        let fetches = backend.fetches.load(Ordering::SeqCst);
        handle.notice_kid(ISS, "k3");
        handle.notice_kid("https://not-published", "x");
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(backend.fetches.load(Ordering::SeqCst), fetches);
        let noticed = Instant::now();
        handle.notice_kid(ISS, "k10");
        attempts(&handle, 6).await;
        assert!(
            Instant::now() - noticed >= Duration::from_secs(10),
            "within a minute of the last unknown-kid refresh"
        );
        assert_eq!(backend.fetches.load(Ordering::SeqCst), fetches + 1);
    }
}
