//! Ticket base58 round-trip + accessors.

use aster::{Credential, NamespaceId, NodeAddr, NodeId, Ticket};

#[test]
fn base58_round_trip_with_addresses() {
    let node_id = NodeId::from_hex("ab".repeat(32));
    let directs = vec!["127.0.0.1:9000".to_string(), "10.0.0.1:9001".to_string()];
    let ticket = Ticket::new(&node_id, Some("1.2.3.4:7777"), &directs, Credential::Open).unwrap();

    let encoded = ticket.to_base58().unwrap();
    let back = Ticket::from_base58(&encoded).unwrap();

    assert_eq!(back.node_id(), node_id);
    assert_eq!(back.relay().as_deref(), Some("1.2.3.4:7777"));
    assert_eq!(back.direct_addresses(), directs);
    assert_eq!(back.credential(), Credential::Open);

    // to_node_addr carries all dialable material — id, direct addrs, and the
    // relay reconstructed as a dialable URL.
    let na = back.to_node_addr().unwrap();
    assert_eq!(na.node_id, node_id);
    assert_eq!(na.direct_addresses, directs);
    assert_eq!(na.relay_url.as_deref(), Some("https://1.2.3.4:7777"));
}

#[test]
fn from_node_addr_round_trips_relay_material() {
    // A peer's address with an IP-based relay (as a self-hosted relay or the
    // compact format intends) plus a direct address.
    let node_id = NodeId::from_hex("ab".repeat(32));
    let addr = NodeAddr {
        node_id: node_id.clone(),
        relay_url: Some("https://1.2.3.4:7777".to_string()),
        direct_addresses: vec!["192.168.1.5:9000".to_string()],
    };

    let ticket = Ticket::from_node_addr(&addr, Credential::Open).unwrap();
    let back = Ticket::from_base58(&ticket.to_base58().unwrap()).unwrap();

    // The relay survived encode/decode — not just the direct addresses.
    assert_eq!(back.relay().as_deref(), Some("1.2.3.4:7777"));
    let rebuilt = back.to_node_addr().unwrap();
    assert_eq!(rebuilt.node_id, node_id);
    assert_eq!(rebuilt.relay_url.as_deref(), Some("https://1.2.3.4:7777"));
    assert_eq!(
        rebuilt.direct_addresses,
        vec!["192.168.1.5:9000".to_string()]
    );
}

#[test]
fn from_node_addr_hostname_relay_defers_to_discovery() {
    // A DNS-hostname relay has no IP to store in the compact slot; Aster drops
    // it (discovery-by-id covers it) but still carries the direct addresses.
    let addr = NodeAddr {
        node_id: NodeId::from_hex("ab".repeat(32)),
        relay_url: Some("https://relay.example.com./".to_string()),
        direct_addresses: vec!["192.168.1.5:9000".to_string()],
    };
    let ticket = Ticket::from_node_addr(&addr, Credential::Open).unwrap();
    assert!(ticket.relay().is_none());
    assert_eq!(
        ticket.direct_addresses(),
        vec!["192.168.1.5:9000".to_string()]
    );
}

#[test]
fn carries_registry_read_credential() {
    let node_id = NodeId::from_hex("cd".repeat(32));
    let ns = NamespaceId::from_bytes([7u8; 32]);
    let ticket = Ticket::new(&node_id, None, &[], Credential::RegistryRead(ns)).unwrap();

    let back = Ticket::from_base58(&ticket.to_base58().unwrap()).unwrap();
    assert_eq!(back.credential(), Credential::RegistryRead(ns));
}

#[test]
fn rejects_bad_address() {
    let node_id = NodeId::from_hex("ab".repeat(32));
    assert!(Ticket::new(
        &node_id,
        None,
        &["not-an-addr".to_string()],
        Credential::Open
    )
    .is_err());
}
