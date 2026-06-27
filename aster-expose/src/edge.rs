//! Public reverse-proxy edge — Case 2 data plane, **cut 1 = H1/H2**
//! (design doc §7, step 10). A Salvo [`Handler`] that reverse-proxies browser
//! HTTP through Aster: look up the request's hostname in the routing table,
//! open one Aster http-relay stream to the origin via
//! [`relay_request_streaming`](crate::relay::relay_request_streaming), and
//! stream the response back as a Salvo `ResBody`.
//!
//! Built on the Salvo fork — hyper-based (not a second HTTP stack), and the
//! *same* stack that carries H3/WT for V2 (a listener swap, not a new stack).
//! Gated behind the `edge` cargo feature so origin-only consumers don't pull in
//! Salvo.
//!
//! The routing table ([`EdgeRouter`]) is populated by the control plane
//! (`register_with_edge`, step 3); this module is just the data plane.

use http_body_util::BodyExt;
use salvo::acme::AcmeListener; // trait providing `TcpListener::acme()`
use salvo::conn::rustls::{Keycert, RustlsConfig};
use salvo::handler::ArcHandler;
use salvo::http::{ResBody, StatusCode};
use salvo::prelude::{Listener, Router, Server, TcpListener};
use salvo::{async_trait, Depot, FlowCtrl, Handler, Request, Response};

use crate::control::{EdgeRouter, RouteProtocol};
use crate::relay::relay_request_streaming;

/// Hop-by-hop headers (RFC 7230 §6.1) — never forwarded across a proxy hop. The
/// relay re-frames the body itself (length-agnostic, EOF-terminated), so
/// `content-length`/`transfer-encoding` must not leak to the origin either.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "content-length",
];

fn is_hop_by_hop(name: &http::HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

/// Resolve the target hostname: the `:authority` (H2/H3, absolute-form URIs)
/// first, then the `Host` header (H1). Port is stripped.
fn request_host(req: &Request) -> Option<String> {
    if let Some(authority) = req.uri().authority() {
        return Some(authority.host().to_ascii_lowercase());
    }
    let host = req.headers().get(http::header::HOST)?.to_str().ok()?;
    Some(host.split(':').next().unwrap_or(host).to_ascii_lowercase())
}

/// Build the outbound relay request's headers from the inbound ones, dropping
/// hop-by-hop headers. When `rewrite_host` is set, the inbound `Host`/`Origin`
/// are replaced (covers strict-origin backends — design doc §6).
fn forward_headers(
    mut builder: http::request::Builder,
    src: &http::HeaderMap,
    rewrite_host: Option<&str>,
) -> http::request::Builder {
    if let Some(headers) = builder.headers_mut() {
        for (name, value) in src {
            if is_hop_by_hop(name) {
                continue;
            }
            // When rewriting, drop the inbound host/origin; set them below.
            if rewrite_host.is_some()
                && (name == http::header::HOST || name == http::header::ORIGIN)
            {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if let Some(h) = rewrite_host {
            if let Ok(v) = http::HeaderValue::from_str(h) {
                headers.insert(http::header::HOST, v);
            }
            if let Ok(v) = http::HeaderValue::from_str(&format!("https://{h}")) {
                headers.insert(http::header::ORIGIN, v);
            }
        }
    }
    builder
}

/// The edge's request handler: one Salvo request → one Aster relay stream.
pub struct RelayHandler {
    router: EdgeRouter,
    /// The port this listener is bound to, used to resolve `(host, port)`
    /// routes. `0` matches default-port registrations.
    port: u16,
    /// If set, the outbound `Host`/`Origin` are rewritten to this value.
    rewrite_host: Option<String>,
}

impl RelayHandler {
    /// Handler resolving routes on the listener's default port (`0`).
    pub fn new(router: EdgeRouter) -> Self {
        Self {
            router,
            port: 0,
            rewrite_host: None,
        }
    }

    /// Handler resolving routes for a specific listener `port` (e.g. 443),
    /// falling back to default-port registrations.
    pub fn with_port(router: EdgeRouter, port: u16) -> Self {
        Self {
            router,
            port,
            rewrite_host: None,
        }
    }
}

#[async_trait]
impl Handler for RelayHandler {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        let Some(host) = request_host(req) else {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render("missing Host / :authority");
            return;
        };
        let Some(route) = self.router.lookup(&host, self.port) else {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(format!("no Aster route for host {host}"));
            return;
        };
        // This listener serves HTTP; a raw-TCP route can't ride the object
        // relay (it needs the L4 splice path on a TCP listener — not wired in
        // cut 1).
        if route.protocol != RouteProtocol::Http {
            res.status_code(StatusCode::BAD_GATEWAY);
            res.render(format!("route for {host} is not HTTP"));
            return;
        }

        // Rebuild the request for the relay, dropping hop-by-hop headers and
        // optionally rewriting Host/Origin.
        let body = req.take_body();
        let builder = http::Request::builder()
            .method(req.method().clone())
            .uri(req.uri().clone())
            .version(req.version());
        let builder = forward_headers(builder, req.headers(), self.rewrite_host.as_deref());
        let outreq = match builder.body(body) {
            Ok(r) => r,
            Err(e) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(format!("bad request: {e}"));
                return;
            }
        };

        match relay_request_streaming(&route.conn, &route.service_id, outreq).await {
            Ok(resp) => {
                let (parts, body) = resp.into_parts();
                res.status_code(parts.status);
                for (name, value) in &parts.headers {
                    if !is_hop_by_hop(name) {
                        res.headers_mut().append(name.clone(), value.clone());
                    }
                }
                // Stream the origin's body straight back — no buffering.
                res.body(ResBody::stream(body.into_data_stream()));
            }
            Err(e) => {
                tracing::debug!("edge relay error for {host}: {e}");
                res.status_code(StatusCode::BAD_GATEWAY);
                res.render(format!("relay error: {e}"));
            }
        }
    }
}

// ── public listener ──────────────────────────────────────────────────────────

/// TLS mode for the public edge listener.
pub enum EdgeTls {
    /// Plaintext HTTP — dev, or behind a separate terminator.
    None,
    /// Static certificate: PEM-encoded cert chain + private key.
    Static { cert_pem: Vec<u8>, key_pem: Vec<u8> },
    /// Automatic Let's Encrypt over ACME HTTP-01. Also binds `:80` for the
    /// challenge. `staging` uses the LE staging directory (untrusted certs, no
    /// rate limits) for testing.
    Acme {
        domains: Vec<String>,
        cache_path: String,
        staging: bool,
    },
}

/// Configuration for [`serve_edge`].
pub struct EdgeConfig {
    /// Bind address for the HTTP(S) listener, e.g. `"0.0.0.0:443"`.
    pub addr: String,
    /// Listener port used to resolve `(host, port)` routes (e.g. 443). `0`
    /// matches default-port registrations.
    pub route_port: u16,
    /// If set, rewrite the outbound `Host`/`Origin` before relaying. Off by
    /// default.
    pub rewrite_host: Option<String>,
    /// Salvo middleware run before the relay (rate-limit / auth / CORS / …).
    /// Applied to the proxy route only, so ACME challenge routes stay open.
    pub hoops: Vec<ArcHandler>,
    pub tls: EdgeTls,
}

impl EdgeConfig {
    /// Plaintext edge bound at `addr`, default-port route matching, no rewrite.
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            route_port: 0,
            rewrite_host: None,
            hoops: Vec::new(),
            tls: EdgeTls::None,
        }
    }

    pub fn route_port(mut self, port: u16) -> Self {
        self.route_port = port;
        self
    }

    pub fn rewrite_host(mut self, host: impl Into<String>) -> Self {
        self.rewrite_host = Some(host.into());
        self
    }

    /// Add a Salvo middleware (`Handler`) in front of the relay — the whole
    /// fork ecosystem (rate-limiter, jwt-auth, cors, …) or a custom hoop.
    pub fn hoop<H: Handler>(mut self, hoop: H) -> Self {
        self.hoops.push(hoop.arc());
        self
    }

    pub fn tls(mut self, tls: EdgeTls) -> Self {
        self.tls = tls;
        self
    }
}

fn catch_all(handler: RelayHandler, hoops: Vec<ArcHandler>) -> Router {
    let mut proxy = Router::with_path("{**path}");
    for hoop in hoops {
        proxy = proxy.hoop(hoop);
    }
    proxy.goal(handler)
}

/// Run the public edge: bind the listener per `config.tls` and reverse-proxy
/// browser HTTP(S) into Aster. Returns when the server stops.
pub async fn serve_edge(routes: EdgeRouter, config: EdgeConfig) {
    let EdgeConfig {
        addr,
        route_port,
        rewrite_host,
        hoops,
        tls,
    } = config;
    let handler = RelayHandler {
        router: routes,
        port: route_port,
        rewrite_host,
    };

    match tls {
        EdgeTls::None => {
            let acceptor = TcpListener::new(addr).bind().await;
            Server::new(acceptor).serve(catch_all(handler, hoops)).await;
        }
        EdgeTls::Static { cert_pem, key_pem } => {
            let tls = RustlsConfig::new(
                Keycert::new()
                    .cert(cert_pem.as_slice())
                    .key(key_pem.as_slice()),
            );
            let acceptor = TcpListener::new(addr).rustls(tls).bind().await;
            Server::new(acceptor).serve(catch_all(handler, hoops)).await;
        }
        EdgeTls::Acme {
            domains,
            cache_path,
            staging,
        } => {
            // Challenge routes must be siblings *before* the catch-all so the
            // wildcard doesn't shadow `/.well-known/acme-challenge` (and the
            // hoops, scoped to the proxy route, never block the challenge).
            let mut app = Router::new();
            let mut listener = TcpListener::new(addr).acme().cache_path(cache_path);
            for d in domains {
                listener = listener.add_domain(d);
            }
            if staging {
                listener = listener.directory("letsencrypt", salvo::acme::LETS_ENCRYPT_STAGING);
            }
            let listener = listener.http01_challenge(&mut app);
            let app = app.push(catch_all(handler, hoops));
            let acceptor = listener.join(TcpListener::new("0.0.0.0:80")).bind().await;
            Server::new(acceptor).serve(app).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_headers_drops_hop_by_hop_and_can_rewrite_host() {
        let mut src = http::HeaderMap::new();
        src.insert(http::header::HOST, "edge.public".parse().unwrap());
        src.insert("connection", "keep-alive".parse().unwrap());
        src.insert("content-length", "10".parse().unwrap());
        src.insert("x-keep", "yes".parse().unwrap());

        // Pass-through: host preserved, hop-by-hop dropped.
        let b = forward_headers(http::Request::builder(), &src, None);
        let h = b.headers_ref().unwrap();
        assert_eq!(h.get(http::header::HOST).unwrap(), "edge.public");
        assert_eq!(h.get("x-keep").unwrap(), "yes");
        assert!(h.get("connection").is_none());
        assert!(h.get("content-length").is_none());

        // Rewrite: host replaced, origin synthesized.
        let b = forward_headers(http::Request::builder(), &src, Some("backend.internal"));
        let h = b.headers_ref().unwrap();
        assert_eq!(h.get(http::header::HOST).unwrap(), "backend.internal");
        assert_eq!(
            h.get(http::header::ORIGIN).unwrap(),
            "https://backend.internal"
        );
    }

    #[test]
    fn empty_router_misses() {
        let router = EdgeRouter::new();
        assert!(router.lookup("a.example", 0).is_none());
        // (No CoreConnection in a unit test — the register/relay path is
        //  covered by tests/edge_integration.rs.)
    }

    #[test]
    fn hop_by_hop_detection() {
        for h in ["connection", "Transfer-Encoding", "CONTENT-LENGTH", "te"] {
            assert!(is_hop_by_hop(
                &http::HeaderName::from_bytes(h.as_bytes()).unwrap()
            ));
        }
        for h in ["host", "x-custom", "content-type"] {
            assert!(!is_hop_by_hop(
                &http::HeaderName::from_bytes(h.as_bytes()).unwrap()
            ));
        }
    }
}
