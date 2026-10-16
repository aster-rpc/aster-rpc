//! A stand-in OpenID provider, in process: one ES256 signing key, its JWK set
//! served over HTTPS from a throwaway CA (the edge's issuer-key publisher
//! fetches it with TLS validated against that CA only), and a sign-in that
//! hands out ID tokens. A real deployment points `[identity.publisher]` at the
//! provider's `jwks_uri` instead and gets tokens from its sign-in redirect.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyUsagePurpose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The provider's issuer (`iss`).
pub const ISSUER: &str = "https://idp.gateway-example.test";
/// The audience its tokens carry: the edge's OIDC client id, which the
/// service lists in `[[identity.trusted_issuers]] audiences`.
pub const AUDIENCE: &str = "notes-edge";
/// The key id of its one signing key.
pub const KID: &str = "idp-key-1";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The running provider.
pub struct TestIdp {
    key: EcdsaKeyPair,
    addr: SocketAddr,
    ca_pem: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestIdp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TestIdp {
    /// Generate the key and a CA, write the CA certificate to `dir`, and serve
    /// the JWK set on `127.0.0.1` (any port).
    pub async fn start(dir: &Path) -> std::io::Result<Self> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING)
            .map_err(|_| std::io::Error::other("key generation failed"))?;
        let jwks = jwks(&key);

        let (server_config, ca_pem) = tls(dir)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let body = jwks.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) if request.len() + n > 16 * 1024 => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(body.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        Ok(Self {
            key,
            addr,
            ca_pem,
            task,
        })
    }

    /// Where the JWK set is served.
    pub fn jwks_url(&self) -> String {
        format!("https://{}/.well-known/jwks.json", self.addr)
    }

    /// The CA certificate (PEM file) the publisher trusts.
    pub fn ca_pem(&self) -> &Path {
        &self.ca_pem
    }

    /// Sign `user` in: an ID token valid for five minutes, with `groups`.
    pub fn sign_in(&self, user: &str, groups: &[&str]) -> String {
        let header = serde_json::json!({"alg": "ES256", "kid": KID, "typ": "JWT"});
        let issued = now();
        let claims = serde_json::json!({
            "iss": ISSUER,
            "sub": user,
            "aud": AUDIENCE,
            "iat": issued,
            "exp": issued + 300,
            "jti": URL_SAFE_NO_PAD.encode(&crate::webauthn::new_challenge()[..12]),
            "amr": ["pwd"],
            "groups": groups,
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let signature = self
            .key
            .sign(&SystemRandom::new(), input.as_bytes())
            .expect("signing failed");
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }
}

fn jwks(key: &EcdsaKeyPair) -> String {
    let point = key.public_key().as_ref();
    serde_json::json!({"keys": [{
        "kty": "EC", "crv": "P-256", "kid": KID, "alg": "ES256", "use": "sig",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    }]})
    .to_string()
}

/// A CA and a `127.0.0.1` leaf it signs; the CA certificate is written to
/// `dir/idp-ca.pem`.
fn tls(dir: &Path) -> std::io::Result<(rustls::ServerConfig, PathBuf)> {
    let fail = |error: &dyn std::fmt::Display| std::io::Error::other(error.to_string());
    let ca_key = rcgen::KeyPair::generate().map_err(|e| fail(&e))?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(|e| fail(&e))?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "gateway example identity provider CA");
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).map_err(|e| fail(&e))?;
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf_key = rcgen::KeyPair::generate().map_err(|e| fail(&e))?;
    let leaf = CertificateParams::new(vec!["127.0.0.1".to_string()])
        .map_err(|e| fail(&e))?
        .signed_by(&leaf_key, &issuer)
        .map_err(|e| fail(&e))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| fail(&e))?
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone(), ca_cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                leaf_key.serialize_der(),
            )),
        )
        .map_err(|e| fail(&e))?;
    let ca_pem = dir.join("idp-ca.pem");
    std::fs::write(&ca_pem, ca_cert.pem())?;
    Ok((config, ca_pem))
}
