//! Node network settings for clusters and air-gapped installs (Kasm A6).
//!
//! Three endpoint settings that [`CoreEndpointConfig`] lowers onto the iroh
//! endpoint builder, next to the bind addresses in `apply_bind_addrs`:
//!
//! - **Advertised addresses** ([`CoreEndpointConfig::advertise_addrs`]):
//!   extra direct addresses the node can be reached on but cannot discover
//!   itself, such as a Kubernetes NodePort or a load balancer. They go to
//!   iroh's `Builder::external_addr`, so they are published with the
//!   discovered direct addresses (in [`Endpoint::addr`](iroh::Endpoint::addr)
//!   and every address lookup record) and peers try them like any other.
//!   They are hints only: a dial still verifies the node's key in the TLS
//!   handshake, so a wrong or stale address cannot reach another node.
//! - **Relay CA trust** ([`CoreEndpointConfig::relay_ca_cert_files`],
//!   [`CoreEndpointConfig::relay_ca_only`]): extra trust anchors for the TLS
//!   that iroh speaks to external services, that is relays and, with custom
//!   discovery, pkarr relays and DNS-over-HTTPS. Applied through iroh's
//!   `Builder::ca_tls_config`. Peer-to-peer connections never use these roots;
//!   their identity is the node key.
//! - **Discovery** ([`CoreEndpointConfig::discovery`]): the address lookup
//!   services that publish this node's addresses and resolve a bare node ID.
//!   `default` is iroh's n0 preset (pkarr and DNS at `iroh.link`); `none`
//!   removes every one of them, `custom` uses only the given pkarr relays and
//!   DNS origin. With `none` or `custom` no lookup reaches n0 infrastructure;
//!   the relay setting is separate (`relay_mode = "default"` still uses the
//!   n0 relays).

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use iroh::address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver};
use iroh::tls::CaTlsConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;
use url::Url;

use crate::CoreEndpointConfig;

/// Address lookup services an endpoint is built with
/// ([`CoreEndpointConfig::discovery`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryPlan {
    /// iroh's n0 preset: pkarr publish/resolve and DNS lookup at `iroh.link`.
    Default,
    /// No address lookup: peers must be dialed by full address.
    None,
    /// Only these services: each pkarr relay publishes and resolves; the DNS
    /// origin resolves `_iroh.<id>.<origin>` TXT records.
    Custom {
        dns_origin: Option<String>,
        pkarr_relays: Vec<Url>,
    },
}

impl DiscoveryPlan {
    /// Every host this plan contacts, for checks such as "nothing reaches
    /// n0". `Default` reports the n0 hosts iroh's preset uses.
    pub fn hosts(&self) -> Vec<String> {
        match self {
            Self::Default => vec!["dns.iroh.link".into(), "iroh.link".into()],
            Self::None => Vec::new(),
            Self::Custom {
                dns_origin,
                pkarr_relays,
            } => pkarr_relays
                .iter()
                .filter_map(|u| u.host_str().map(str::to_string))
                .chain(dns_origin.iter().cloned())
                .collect(),
        }
    }

    /// Number of address lookup services this plan installs.
    pub fn service_count(&self) -> usize {
        match self {
            Self::Default => 3,
            Self::None => 0,
            Self::Custom {
                dns_origin,
                pkarr_relays,
            } => pkarr_relays.len() * 2 + usize::from(dns_origin.is_some()),
        }
    }
}

/// Resolve [`CoreEndpointConfig::discovery`] and its custom servers.
///
/// `discovery_dns_origin` / `discovery_pkarr_relays` alone select `custom`;
/// with another mode they are an error, and `custom` needs at least one.
pub fn discovery_plan(config: &CoreEndpointConfig) -> Result<DiscoveryPlan> {
    let origin = config
        .discovery_dns_origin
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(|o| o.trim_end_matches('.').to_string());
    let has_custom = origin.is_some() || !config.discovery_pkarr_relays.is_empty();
    let mode = config.discovery.as_deref().map(str::trim);
    match mode {
        None | Some("") if !has_custom => Ok(DiscoveryPlan::Default),
        Some("default") | Some("none") if has_custom => Err(anyhow!(
            "discovery_dns_origin / discovery_pkarr_relays require discovery \"custom\", got {:?}",
            mode.unwrap_or_default()
        )),
        Some("default") => Ok(DiscoveryPlan::Default),
        Some("none") => Ok(DiscoveryPlan::None),
        None | Some("") | Some("custom") => {
            if !has_custom {
                return Err(anyhow!(
                    "discovery \"custom\" requires discovery_dns_origin or discovery_pkarr_relays"
                ));
            }
            let pkarr_relays = config
                .discovery_pkarr_relays
                .iter()
                .map(|raw| {
                    let url: Url = raw
                        .trim()
                        .parse()
                        .map_err(|e| anyhow!("invalid discovery_pkarr_relays URL '{raw}': {e}"))?;
                    match url.scheme() {
                        "https" | "http" => Ok(url),
                        other => Err(anyhow!(
                            "discovery_pkarr_relays URL '{raw}' must be http(s), not {other}"
                        )),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(DiscoveryPlan::Custom {
                dns_origin: origin,
                pkarr_relays,
            })
        }
        Some(other) => Err(anyhow!(
            "unsupported discovery: {other:?} (expected default, none or custom)"
        )),
    }
}

/// Parse [`CoreEndpointConfig::advertise_addrs`]: each a socket address with a
/// specific IP and a non-zero port.
pub fn advertise_addrs(config: &CoreEndpointConfig) -> Result<Vec<SocketAddr>> {
    config
        .advertise_addrs
        .iter()
        .map(|raw| {
            let addr: SocketAddr = raw.trim().parse().map_err(|e| {
                anyhow!("invalid advertise_addrs entry '{raw}': {e} (expected IP:port)")
            })?;
            if addr.ip().is_unspecified() || addr.port() == 0 {
                return Err(anyhow!(
                    "advertise_addrs entry '{raw}' needs a specific IP and a non-zero port"
                ));
            }
            Ok(addr)
        })
        .collect()
}

/// Read every certificate from the PEM files in
/// [`CoreEndpointConfig::relay_ca_cert_files`].
fn read_ca_certs(files: &[String]) -> Result<Vec<CertificateDer<'static>>> {
    let mut certs = Vec::new();
    for file in files {
        let path = Path::new(file);
        let bytes =
            std::fs::read(path).with_context(|| format!("relay_ca_certs: cannot read {file}"))?;
        let before = certs.len();
        for cert in CertificateDer::pem_slice_iter(&bytes) {
            certs.push(cert.map_err(|e| anyhow!("relay_ca_certs: {file}: invalid PEM: {e}"))?);
        }
        if certs.len() == before {
            return Err(anyhow!(
                "relay_ca_certs: {file} holds no PEM CERTIFICATE block"
            ));
        }
    }
    Ok(certs)
}

/// The CA trust for relay (and other non-peer) TLS, or `None` for iroh's
/// default (the built-in WebPKI roots).
pub fn relay_ca_tls_config(config: &CoreEndpointConfig) -> Result<Option<CaTlsConfig>> {
    if config.relay_ca_cert_files.is_empty() {
        if config.relay_ca_only {
            return Err(anyhow!(
                "relay_ca_only requires at least one relay_ca_certs file"
            ));
        }
        return Ok(None);
    }
    let certs = read_ca_certs(&config.relay_ca_cert_files)?;
    // Fail at start, not on the first relay handshake, if a file holds
    // something rustls will not accept as a trust anchor.
    let mut store = rustls::RootCertStore::empty();
    let (_, rejected) = store.add_parsable_certificates(certs.iter().cloned());
    if rejected != 0 {
        return Err(anyhow!(
            "relay_ca_certs: {rejected} certificate(s) are not usable trust anchors"
        ));
    }
    Ok(Some(if config.relay_ca_only {
        CaTlsConfig::custom_roots(certs)
    } else {
        CaTlsConfig::embedded().with_extra_roots(certs)
    }))
}

/// Apply advertised addresses, relay CA trust and discovery to `builder`,
/// which was created from iroh's n0 preset.
pub(crate) fn apply_network_settings(
    mut builder: iroh::endpoint::Builder,
    config: &CoreEndpointConfig,
) -> Result<iroh::endpoint::Builder> {
    let advertised = advertise_addrs(config)?;
    if !advertised.is_empty() && config.clear_ip_transports {
        return Err(anyhow!(
            "advertise_addrs needs direct IP transports, but clear_ip_transports is set"
        ));
    }
    for addr in advertised {
        builder = builder.external_addr(addr);
    }

    if let Some(ca) = relay_ca_tls_config(config)? {
        builder = builder.ca_tls_config(ca);
    }

    match discovery_plan(config)? {
        DiscoveryPlan::Default => {}
        DiscoveryPlan::None => builder = builder.clear_address_lookup(),
        DiscoveryPlan::Custom {
            dns_origin,
            pkarr_relays,
        } => {
            builder = builder.clear_address_lookup();
            for relay in pkarr_relays {
                builder = builder
                    .address_lookup(PkarrPublisher::builder(relay.clone()))
                    .address_lookup(PkarrResolver::builder(relay));
            }
            if let Some(origin) = dns_origin {
                builder = builder.address_lookup(DnsAddressLookup::builder(origin));
            }
        }
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CoreEndpointConfig {
        CoreEndpointConfig::default()
    }

    #[test]
    fn discovery_modes() {
        assert_eq!(discovery_plan(&cfg()).unwrap(), DiscoveryPlan::Default);
        let mut c = cfg();
        c.discovery = Some("none".into());
        assert_eq!(discovery_plan(&c).unwrap(), DiscoveryPlan::None);

        let mut c = cfg();
        c.discovery_dns_origin = Some("dns.example.internal.".into());
        c.discovery_pkarr_relays = vec!["https://dns.example.internal/pkarr".into()];
        let plan = discovery_plan(&c).unwrap();
        assert_eq!(
            plan,
            DiscoveryPlan::Custom {
                dns_origin: Some("dns.example.internal".into()),
                pkarr_relays: vec!["https://dns.example.internal/pkarr".parse().unwrap()],
            }
        );
        assert_eq!(plan.service_count(), 3);

        c.discovery = Some("custom".into());
        assert!(discovery_plan(&c).is_ok());
        c.discovery = Some("none".into());
        assert!(discovery_plan(&c).is_err());

        let mut c = cfg();
        c.discovery = Some("custom".into());
        assert!(discovery_plan(&c).is_err(), "custom without servers");
        c.discovery = Some("mdns".into());
        assert!(discovery_plan(&c).is_err());
        let mut c = cfg();
        c.discovery_pkarr_relays = vec!["ftp://x.example".into()];
        assert!(discovery_plan(&c).is_err());
    }

    /// Air-gapped settings never name an n0 host.
    #[test]
    fn none_and_custom_never_reach_n0() {
        let mut none = cfg();
        none.discovery = Some("none".into());
        let mut custom = cfg();
        custom.discovery_pkarr_relays = vec!["https://pkarr.example.internal/pkarr".into()];
        custom.discovery_dns_origin = Some("dns.example.internal".into());
        for c in [none, custom] {
            for host in discovery_plan(&c).unwrap().hosts() {
                assert!(
                    !host.ends_with("iroh.link") && !host.ends_with("n0.computer"),
                    "{host}"
                );
            }
        }
        assert!(DiscoveryPlan::Default
            .hosts()
            .iter()
            .any(|h| h.ends_with("iroh.link")));
    }

    #[test]
    fn advertise_addrs_validation() {
        let mut c = cfg();
        c.advertise_addrs = vec!["203.0.113.10:31842".into(), "[2001:db8::1]:7842".into()];
        assert_eq!(advertise_addrs(&c).unwrap().len(), 2);
        for bad in ["203.0.113.10", "0.0.0.0:7842", "203.0.113.10:0", "host:1"] {
            c.advertise_addrs = vec![bad.into()];
            assert!(advertise_addrs(&c).is_err(), "{bad}");
        }
    }

    #[test]
    fn relay_ca_files() {
        let mut c = cfg();
        assert!(relay_ca_tls_config(&c).unwrap().is_none());
        c.relay_ca_only = true;
        assert!(relay_ca_tls_config(&c).is_err(), "ca_only without files");

        let dir = std::env::temp_dir().join(format!("aster-relay-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "not a certificate\n").unwrap();
        c.relay_ca_cert_files = vec![empty.to_string_lossy().into_owned()];
        let err = relay_ca_tls_config(&c).unwrap_err().to_string();
        assert!(err.contains("no PEM CERTIFICATE"), "{err}");
        c.relay_ca_cert_files = vec![dir.join("missing.pem").to_string_lossy().into_owned()];
        assert!(relay_ca_tls_config(&c).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
