//! TLS for the HTTP transport — three data-only modes (FFI-friendly) plus
//! self-signed generation for WebTransport `serverCertificateHashes`. See the
//! "TLS / certificate provisioning" section of the design doc.
//!
//! - [`TlsMaterial::Pem`] — operator-supplied cert/key.
//! - [`TlsMaterial::Acme`] — ACME / Let's Encrypt (TLS-ALPN-01, single port).
//! - [`TlsMaterial::SelfSigned`] — generated (ECDSA), for dev + pinned mesh.
//!
//! [`serve_https`] is the batteries-included entry; [`rustls_config`] and
//! [`generate_self_signed`] are the building blocks for custom wiring.

use std::path::PathBuf;

use salvo::conn::rustls::{Keycert, RustlsConfig};
use salvo::conn::{QuinnListener, TcpListener};
use salvo::prelude::*;
use sha2::{Digest, Sha256};

/// Where the server's TLS certificate comes from. Data only — no closures — so
/// it crosses the FFI boundary.
#[derive(Clone, Debug)]
pub enum TlsMaterial {
    /// Operator-supplied PEM (cert chain + private key), as bytes.
    Pem { cert_pem: Vec<u8>, key_pem: Vec<u8> },
    /// ACME / Let's Encrypt via TLS-ALPN-01 on the same listener (public
    /// domains). `cache_dir` persists the account + issued certs across
    /// restarts. Untestable without a real domain.
    Acme {
        domains: Vec<String>,
        contact_email: Option<String>,
        cache_dir: PathBuf,
    },
    /// Generated self-signed cert (ECDSA). For inner-loop dev and pinned-cert
    /// mesh; pair with [`generate_self_signed`] to publish the hash for
    /// WebTransport `serverCertificateHashes`.
    SelfSigned { sans: Vec<String> },
}

impl TlsMaterial {
    /// Operator PEM from bytes (or read files yourself).
    pub fn pem(cert_pem: impl Into<Vec<u8>>, key_pem: impl Into<Vec<u8>>) -> Self {
        Self::Pem {
            cert_pem: cert_pem.into(),
            key_pem: key_pem.into(),
        }
    }

    /// A self-signed cert for the given SANs (defaults to `localhost`).
    pub fn self_signed(sans: impl IntoIterator<Item = String>) -> Self {
        Self::SelfSigned {
            sans: sans.into_iter().collect(),
        }
    }
}

/// A generated self-signed certificate: PEM for the server, plus the SHA-256 of
/// the DER — the value a browser pins via WebTransport `serverCertificateHashes`
/// (publish it in the Aster ticket alongside the node id).
pub struct GeneratedCert {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// SHA-256 of the certificate DER.
    pub sha256: [u8; 32],
}

/// Generate a self-signed ECDSA cert for `sans` (empty → `["localhost"]`).
pub fn generate_self_signed(sans: &[String]) -> Result<GeneratedCert, String> {
    let sans = if sans.is_empty() {
        vec!["localhost".to_string()]
    } else {
        sans.to_vec()
    };
    let ck = rcgen::generate_simple_self_signed(sans).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(ck.cert.der().as_ref());
    Ok(GeneratedCert {
        cert_pem: ck.cert.pem().into_bytes(),
        key_pem: ck.key_pair.serialize_pem().into_bytes(),
        sha256: hasher.finalize().into(),
    })
}

/// Build a Salvo [`RustlsConfig`] from PEM or a freshly generated self-signed
/// cert. (ACME isn't a static config — use [`serve_https`].)
pub fn rustls_config(tls: &TlsMaterial) -> Result<RustlsConfig, String> {
    let (cert_pem, key_pem) = match tls {
        TlsMaterial::Pem { cert_pem, key_pem } => (cert_pem.clone(), key_pem.clone()),
        TlsMaterial::SelfSigned { sans } => {
            let g = generate_self_signed(sans)?;
            (g.cert_pem, g.key_pem)
        }
        TlsMaterial::Acme { .. } => {
            return Err("ACME is not a static RustlsConfig; use serve_https".into())
        }
    };
    Ok(RustlsConfig::new(
        Keycert::new().cert(cert_pem).key(key_pem),
    ))
}

/// Bind `service` over HTTPS on `addr` and serve until the process ends.
///
/// For PEM / self-signed, this serves **H1/H2 over TCP *and* H3 over QUIC** on
/// the same address (one `QuinnListener` joined with the TCP listener) — so the
/// existing handlers, and WebTransport, work over HTTP/3. ACME currently serves
/// H1/H2 only.
///
/// ```ignore
/// let app = aster_transport_salvo::router(dispatcher);
/// aster_transport_salvo::serve_https(
///     "[::]:443",
///     TlsMaterial::self_signed(["localhost".into()]),
///     Service::new(app),
/// ).await?;
/// ```
pub async fn serve_https(addr: &str, tls: TlsMaterial, service: Service) -> Result<(), String> {
    match tls {
        TlsMaterial::Acme {
            domains,
            contact_email,
            cache_dir,
        } => {
            let mut listener = TcpListener::new(addr.to_string())
                .acme()
                .cache_path(cache_dir)
                .tls_alpn01_challenge();
            for d in &domains {
                listener = listener.add_domain(d);
            }
            if let Some(c) = contact_email {
                listener = listener.contacts(vec![format!("mailto:{c}")]);
            }
            let acceptor = listener.bind().await;
            Server::new(acceptor).serve(service).await;
            Ok(())
        }
        other => {
            let config = rustls_config(&other)?;
            // H1/H2 over TCP + H3 over QUIC on the same address.
            let tcp = TcpListener::new(addr.to_string()).rustls(config.clone());
            let quinn_config = config.build_quinn_config().map_err(|e| e.to_string())?;
            let acceptor = QuinnListener::new(quinn_config, addr.to_string())
                .join(tcp)
                .bind()
                .await;
            Server::new(acceptor).serve(service).await;
            Ok(())
        }
    }
}
