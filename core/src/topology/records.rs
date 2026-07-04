//! Topology v2 wire records — the `topo/v1/…` entries of the shared
//! topology namespace.
//!
//! Schema is normative per `docs/_internal/working_ideas/aster-network-topology.md`
//! §"Record schema": Fory XLANG encoding (payload root `aster.topo`),
//! all-integer fields, version in the key path (`topo/v1/…`; unknown
//! `topo/v<N>` prefixes are ignored by readers for forward compatibility).

use anyhow::{anyhow, Result};
use fory_core::Fory;
use fory_derive::ForyStruct;
use std::sync::OnceLock;

/// Key prefix of the current record version. Readers scan this prefix;
/// anything else under `topo/` is a future version and is ignored.
pub const TOPO_KEY_PREFIX: &str = "topo/v1/";

/// A node's self-published position record (`topo/v1/<node>/position`).
///
/// `prefix_hashes` holds concatenated 16-byte salted prefix hashes (see the
/// design doc's Privacy section); empty until the prefix-matching increment
/// lands.
#[derive(ForyStruct, Clone, Debug, Default, PartialEq)]
pub struct NetworkPosition {
    /// 32-byte Ed25519 public key of the publishing node.
    #[fory(id = 1, bytes)]
    pub node_id: Vec<u8>,
    /// Relay-observed public endpoint; "" = unknown. Advisory.
    #[fory(id = 2)]
    pub observed_public: String,
    /// 0 = unknown. Advisory.
    #[fory(id = 3)]
    pub asn: i32,
    /// "" = none. Advisory.
    #[fory(id = 4)]
    pub home_relay: String,
    /// Concatenated 16-byte truncated salted prefix hashes.
    #[fory(id = 5, bytes)]
    pub prefix_hashes: Vec<u8>,
    #[fory(id = 6)]
    pub updated_unix_ms: i64,
}

/// One node's half of a measured RTT edge
/// (`topo/v1/<node>/rtt/<peer>`). Published for every measured pair, near
/// or far — an above-threshold measurement is separation evidence.
#[derive(ForyStruct, Clone, Debug, Default, PartialEq)]
pub struct RttEdge {
    /// Smoothed RTT, microseconds.
    #[fory(id = 1)]
    pub rtt_us: i32,
    #[fory(id = 2)]
    pub samples: i32,
    /// Since when smoothed RTT has continuously held under the cluster
    /// enter threshold; 0 = not currently under. Publisher-owned
    /// hysteresis (set below enter, cleared above exit).
    #[fory(id = 3)]
    pub held_since_unix_ms: i64,
    #[fory(id = 4)]
    pub measured_unix_ms: i64,
}

/// Verified private-path dial attestation (`topo/v1/<node>/lan/<peer>`).
#[derive(ForyStruct, Clone, Debug, Default, PartialEq)]
pub struct LanEdge {
    #[fory(id = 1)]
    pub verified_unix_ms: i64,
}

static FORY: OnceLock<Fory> = OnceLock::new();

fn fory() -> &'static Fory {
    FORY.get_or_init(|| {
        let mut f = Fory::builder().xlang(true).compatible(true).build();
        f.register_by_name::<NetworkPosition>("aster.topo.NetworkPosition")
            .expect("register NetworkPosition");
        f.register_by_name::<RttEdge>("aster.topo.RttEdge")
            .expect("register RttEdge");
        f.register_by_name::<LanEdge>("aster.topo.LanEdge")
            .expect("register LanEdge");
        f
    })
}

pub fn encode_position(p: &NetworkPosition) -> Vec<u8> {
    fory().serialize(p).expect("NetworkPosition Fory serialize")
}

pub fn decode_position(bytes: &[u8]) -> Result<NetworkPosition> {
    let p: NetworkPosition = fory()
        .deserialize(bytes)
        .map_err(|e| anyhow!("NetworkPosition Fory decode failed: {e}"))?;
    if p.node_id.len() != 32 {
        return Err(anyhow!("NetworkPosition node_id must be 32 bytes"));
    }
    Ok(p)
}

pub fn encode_rtt_edge(e: &RttEdge) -> Vec<u8> {
    fory().serialize(e).expect("RttEdge Fory serialize")
}

pub fn decode_rtt_edge(bytes: &[u8]) -> Result<RttEdge> {
    fory()
        .deserialize(bytes)
        .map_err(|e| anyhow!("RttEdge Fory decode failed: {e}"))
}

pub fn encode_lan_edge(e: &LanEdge) -> Vec<u8> {
    fory().serialize(e).expect("LanEdge Fory serialize")
}

pub fn decode_lan_edge(bytes: &[u8]) -> Result<LanEdge> {
    fory()
        .deserialize(bytes)
        .map_err(|e| anyhow!("LanEdge Fory decode failed: {e}"))
}

// ── Key layout ────────────────────────────────────────────────────────────────

/// Parsed `topo/v1/…` key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopoKey {
    /// `topo/v1/<node>/position`
    Position { node: String },
    /// `topo/v1/<node>/rtt/<peer>`
    Rtt { node: String, peer: String },
    /// `topo/v1/<node>/lan/<peer>`
    Lan { node: String, peer: String },
}

impl TopoKey {
    /// The node that owns (must have authored) this key.
    pub fn node(&self) -> &str {
        match self {
            TopoKey::Position { node } => node,
            TopoKey::Rtt { node, .. } => node,
            TopoKey::Lan { node, .. } => node,
        }
    }
}

pub fn position_key(node_hex: &str) -> Vec<u8> {
    format!("{TOPO_KEY_PREFIX}{node_hex}/position").into_bytes()
}

pub fn rtt_key(node_hex: &str, peer_hex: &str) -> Vec<u8> {
    format!("{TOPO_KEY_PREFIX}{node_hex}/rtt/{peer_hex}").into_bytes()
}

pub fn lan_key(node_hex: &str, peer_hex: &str) -> Vec<u8> {
    format!("{TOPO_KEY_PREFIX}{node_hex}/lan/{peer_hex}").into_bytes()
}

fn is_node_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Parse a key from the topology namespace. Returns `None` for anything
/// that isn't a well-formed `topo/v1/…` key (including future `topo/v<N>`
/// versions) — readers skip such entries rather than erroring.
pub fn parse_key(key: &[u8]) -> Option<TopoKey> {
    let key = std::str::from_utf8(key).ok()?;
    let rest = key.strip_prefix(TOPO_KEY_PREFIX)?;
    let mut parts = rest.split('/');
    let node = parts.next()?;
    if !is_node_hex(node) {
        return None;
    }
    let kind = parts.next()?;
    let result = match kind {
        "position" => TopoKey::Position { node: node.into() },
        "rtt" | "lan" => {
            let peer = parts.next()?;
            if !is_node_hex(peer) {
                return None;
            }
            if kind == "rtt" {
                TopoKey::Rtt {
                    node: node.into(),
                    peer: peer.into(),
                }
            } else {
                TopoKey::Lan {
                    node: node.into(),
                    peer: peer.into(),
                }
            }
        }
        _ => return None,
    };
    if parts.next().is_some() {
        return None; // trailing segments — not a v1 key shape we know
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_id(b: u8) -> String {
        hex::encode([b; 32])
    }

    #[test]
    fn record_roundtrips() {
        let p = NetworkPosition {
            node_id: vec![7u8; 32],
            observed_public: "203.0.113.9:4433".into(),
            asn: 64512,
            home_relay: "https://relay.aster0.net".into(),
            prefix_hashes: vec![1u8; 32], // two 16-byte hashes
            updated_unix_ms: 1_700_000_000_000,
        };
        assert_eq!(decode_position(&encode_position(&p)).unwrap(), p);

        let e = RttEdge {
            rtt_us: 12_345,
            samples: 42,
            held_since_unix_ms: 1_700_000_000_000,
            measured_unix_ms: 1_700_000_060_000,
        };
        assert_eq!(decode_rtt_edge(&encode_rtt_edge(&e)).unwrap(), e);

        let l = LanEdge {
            verified_unix_ms: 1_700_000_000_000,
        };
        assert_eq!(decode_lan_edge(&encode_lan_edge(&l)).unwrap(), l);
    }

    #[test]
    fn decode_position_rejects_bad_node_id() {
        let p = NetworkPosition {
            node_id: vec![7u8; 5],
            ..Default::default()
        };
        assert!(decode_position(&encode_position(&p)).is_err());
    }

    #[test]
    fn key_roundtrip_and_parse_table() {
        let a = hex_id(0xaa);
        let b = hex_id(0xbb);

        assert_eq!(
            parse_key(&position_key(&a)),
            Some(TopoKey::Position { node: a.clone() })
        );
        assert_eq!(
            parse_key(&rtt_key(&a, &b)),
            Some(TopoKey::Rtt {
                node: a.clone(),
                peer: b.clone()
            })
        );
        assert_eq!(
            parse_key(&lan_key(&a, &b)),
            Some(TopoKey::Lan {
                node: a.clone(),
                peer: b.clone()
            })
        );

        // Rejects: future version, junk, bad hex, uppercase, truncation,
        // trailing segments, non-utf8.
        assert_eq!(parse_key(format!("topo/v2/{a}/position").as_bytes()), None);
        assert_eq!(parse_key(b"unrelated/key"), None);
        assert_eq!(parse_key(b"topo/v1/nothex/position"), None);
        assert_eq!(
            parse_key(format!("topo/v1/{}/position", a.to_uppercase()).as_bytes()),
            None
        );
        assert_eq!(parse_key(format!("topo/v1/{a}").as_bytes()), None);
        assert_eq!(parse_key(format!("topo/v1/{a}/rtt").as_bytes()), None);
        assert_eq!(parse_key(format!("topo/v1/{a}/unknown").as_bytes()), None);
        assert_eq!(
            parse_key(format!("topo/v1/{a}/position/extra").as_bytes()),
            None
        );
        assert_eq!(parse_key(&[0xff, 0xfe]), None);
    }
}
