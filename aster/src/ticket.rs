//! Compact Aster tickets — a peer's address material (+ optional credential)
//! as a base58 string, for out-of-band sharing (e.g. to bootstrap an admission
//! handshake before a docs join).

use crate::error::{Error, Result};
use crate::id::{NamespaceId, NodeAddr, NodeId};
use aster_transport_core::ticket::{AsterTicket as CoreTicket, TicketCredential};
use std::net::{IpAddr, SocketAddr};
use url::{Host, Url};

/// The credential a ticket may carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// No credential — open access.
    Open,
    /// A consumer RCAN credential (opaque JSON bytes).
    ConsumerRcan(Vec<u8>),
    /// An enrollment credential (opaque JSON bytes).
    Enrollment(Vec<u8>),
    /// Registry read capability: the namespace whose id grants read access.
    RegistryRead(NamespaceId),
}

impl Credential {
    fn to_core(&self) -> TicketCredential {
        match self {
            Credential::Open => TicketCredential::Open,
            Credential::ConsumerRcan(v) => TicketCredential::ConsumerRcan(v.clone()),
            Credential::Enrollment(v) => TicketCredential::Enrollment(v.clone()),
            Credential::RegistryRead(ns) => TicketCredential::RegistryRead(ns.to_bytes()),
        }
    }

    fn from_core(c: Option<TicketCredential>) -> Self {
        match c {
            None | Some(TicketCredential::Open) => Credential::Open,
            Some(TicketCredential::ConsumerRcan(v)) => Credential::ConsumerRcan(v),
            Some(TicketCredential::Enrollment(v)) => Credential::Enrollment(v),
            Some(TicketCredential::RegistryRead(ns)) => {
                Credential::RegistryRead(NamespaceId::from_bytes(ns))
            }
        }
    }
}

/// A compact, shareable ticket: a peer's `node_id` + relay/direct addresses +
/// an optional [`Credential`], encodable as a base58 string.
#[derive(Clone, Debug, PartialEq)]
pub struct Ticket {
    inner: CoreTicket,
}

impl Ticket {
    /// Build a ticket from address material. `relay` and `direct_addresses` are
    /// `ip:port` socket-address strings.
    pub fn new(
        node_id: &NodeId,
        relay: Option<&str>,
        direct_addresses: &[String],
        credential: Credential,
    ) -> Result<Ticket> {
        let id_bytes = hex::decode(node_id.as_str())
            .map_err(|e| Error::InvalidArgument(format!("bad node id hex: {e}")))?;
        let endpoint_id: [u8; 32] = id_bytes
            .try_into()
            .map_err(|_| Error::InvalidArgument("node id must be 32 bytes".into()))?;
        let relay = match relay {
            Some(s) => Some(
                s.parse::<SocketAddr>()
                    .map_err(|e| Error::InvalidArgument(format!("bad relay addr: {e}")))?,
            ),
            None => None,
        };
        let direct_addrs = direct_addresses
            .iter()
            .map(|s| {
                s.parse::<SocketAddr>()
                    .map_err(|e| Error::InvalidArgument(format!("bad direct addr {s}: {e}")))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Ticket {
            inner: CoreTicket {
                endpoint_id,
                relay,
                direct_addrs,
                credential: Some(credential.to_core()),
            },
        })
    }

    /// Build a ticket from a peer's [`NodeAddr`] — the recommended way to mint a
    /// shareable address token from `node.addr()`.
    ///
    /// Aster owns the interpretation of the address material: the node id and
    /// direct addresses are carried verbatim, and the relay is folded in as its
    /// IP:port (the compact wire format stores relay as a socket address — see
    /// [`Ticket::relay`]). A relay advertised as a DNS hostname cannot fit the
    /// compact IP slot, so it is dropped from the ticket and left to
    /// discovery-by-id; the direct addresses still travel in the ticket. The
    /// relay round-trips through [`to_node_addr`](Ticket::to_node_addr).
    pub fn from_node_addr(addr: &NodeAddr, credential: Credential) -> Result<Ticket> {
        let relay = match &addr.relay_url {
            Some(url) => relay_url_to_socket(url)?.map(|s| s.to_string()),
            None => None,
        };
        Ticket::new(
            &addr.node_id,
            relay.as_deref(),
            &addr.direct_addresses,
            credential,
        )
    }

    /// Decode from a base58 ticket string.
    pub fn from_base58(s: &str) -> Result<Ticket> {
        Ok(Ticket {
            inner: CoreTicket::from_base58_str(s)?,
        })
    }

    /// Encode as a base58 ticket string.
    pub fn to_base58(&self) -> Result<String> {
        Ok(self.inner.to_base58_string()?)
    }

    /// The peer's node id.
    pub fn node_id(&self) -> NodeId {
        NodeId::from_hex(hex::encode(self.inner.endpoint_id))
    }

    /// The relay socket address (`ip:port`), if any.
    pub fn relay(&self) -> Option<String> {
        self.inner.relay.map(|s| s.to_string())
    }

    /// The direct socket addresses (`ip:port`).
    pub fn direct_addresses(&self) -> Vec<String> {
        self.inner
            .direct_addrs
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// The carried credential.
    pub fn credential(&self) -> Credential {
        Credential::from_core(self.inner.credential.clone())
    }

    /// Build a [`NodeAddr`] carrying all dialable address material — node id,
    /// direct addresses, and the relay (as a `https://<ip>:<port>` URL
    /// reconstructed from the ticket's stored socket address). Suitable for
    /// [`Node::connect_addr`](crate::Node::connect_addr) /
    /// [`Node::add_node_addr`](crate::Node::add_node_addr); prefer the
    /// [`Node::connect_ticket`](crate::Node::connect_ticket) /
    /// [`Node::add_ticket_addr`](crate::Node::add_ticket_addr) shortcuts.
    pub fn to_node_addr(&self) -> Result<NodeAddr> {
        Ok(NodeAddr {
            node_id: self.node_id(),
            relay_url: self.inner.relay.map(|s| socket_to_relay_url(&s)),
            direct_addresses: self.direct_addresses(),
        })
    }
}

/// Reconstruct a relay URL from the socket address stored in a ticket. The
/// compact format keeps relay as IP:port; `https://<ip>:<port>` is the dialable
/// form iroh parses back into a relay address (IPv6 is bracketed by
/// `SocketAddr`'s `Display`).
fn socket_to_relay_url(addr: &SocketAddr) -> String {
    format!("https://{addr}")
}

/// Extract the IP:port from a peer's relay URL for storage in the compact
/// ticket. Returns `Ok(None)` for a DNS-hostname relay (which has no IP to
/// store — it is left to discovery-by-id), and `Err` only if the URL is
/// malformed.
fn relay_url_to_socket(url: &str) -> Result<Option<SocketAddr>> {
    let parsed = Url::parse(url)
        .map_err(|e| Error::InvalidArgument(format!("bad relay url {url:?}: {e}")))?;
    let ip = match parsed.host() {
        Some(Host::Ipv4(ip)) => IpAddr::V4(ip),
        Some(Host::Ipv6(ip)) => IpAddr::V6(ip),
        // A hostname relay can't fit the compact IP slot; defer to discovery.
        Some(Host::Domain(_)) | None => return Ok(None),
    };
    let port = parsed.port_or_known_default().ok_or_else(|| {
        Error::InvalidArgument(format!(
            "relay url {url:?} has no port and no known default"
        ))
    })?;
    Ok(Some(SocketAddr::new(ip, port)))
}
