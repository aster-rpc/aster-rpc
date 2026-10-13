//! Byte-stability fixtures for every Fory-encoded artefact `core` may persist
//! or exchange (namespace capabilities, HPKE envelopes, grant records,
//! topology records).
//!
//! `fixtures/fory_golden_1_3_0.txt` was captured under `fory-core =1.3.0`
//! (the pin before the workspace moved to 1.7.6). The tests below assert two
//! separate properties against it, so a Fory upgrade reports exactly which of
//! them it breaks:
//!
//! * **encode stability** — the current encoder reproduces the 1.3.0 bytes;
//! * **decode compatibility** — the current decoder still reads the 1.3.0
//!   bytes back to the original value (what persisted data needs).
//!
//! Regenerate (only when a change is intended and recorded):
//! `ASTER_FORY_GOLDEN_WRITE=1 cargo test -p aster_transport_core --test fory_byte_stability`

use std::collections::BTreeMap;
use std::path::PathBuf;

use aster_transport_core::grant_record::GrantRecord;
use aster_transport_core::hpke_envelope::HpkeEnvelope;
use aster_transport_core::namespace::CoreNamespaceCapability;
use aster_transport_core::topology::records::{
    decode_lan_edge, decode_position, decode_rtt_edge, encode_lan_edge, encode_position,
    encode_rtt_edge, LanEdge, NetworkPosition, RttEdge,
};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fory_golden_1_3_0.txt")
}

fn hpke() -> HpkeEnvelope {
    HpkeEnvelope::new(vec![0x33; 32], vec![0x44; 48])
}

fn position() -> NetworkPosition {
    NetworkPosition {
        node_id: vec![0x55; 32],
        observed_public: "203.0.113.7:4433".into(),
        asn: 64_512,
        home_relay: "https://relay.example/".into(),
        prefix_hashes: vec![0x66; 32],
        updated_unix_ms: 1_760_000_000_123,
    }
}

fn rtt() -> RttEdge {
    RttEdge {
        rtt_us: 1_234,
        samples: 17,
        held_since_unix_ms: 1_760_000_000_000,
        measured_unix_ms: 1_760_000_000_999,
    }
}

fn lan() -> LanEdge {
    LanEdge {
        verified_unix_ms: 1_760_000_001_000,
    }
}

/// Current encodings, by fixture name.
fn current() -> BTreeMap<&'static str, Vec<u8>> {
    let mut m = BTreeMap::new();
    m.insert(
        "namespace.read",
        CoreNamespaceCapability::Read([0x11; 32]).encode_fory(),
    );
    m.insert(
        "namespace.write",
        CoreNamespaceCapability::Write([0x22; 32]).encode_fory(),
    );
    m.insert("hpke.envelope", hpke().encode_fory());
    m.insert(
        "grant.active",
        GrantRecord::active("write", 1, hpke().encode_fory()).encode_fory(),
    );
    m.insert(
        "grant.revoked",
        GrantRecord::revoked("read", "rotated").encode_fory(),
    );
    m.insert("topo.position", encode_position(&position()));
    m.insert("topo.rtt", encode_rtt_edge(&rtt()));
    m.insert("topo.lan", encode_lan_edge(&lan()));
    m
}

fn load() -> BTreeMap<String, Vec<u8>> {
    let text = std::fs::read_to_string(fixture_path()).expect("read fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (n, h) = l.split_once(' ').expect("`name hex` line");
            (n.to_string(), hex::decode(h.trim()).expect("hex"))
        })
        .collect()
}

#[test]
fn write_or_compare_encodings() {
    let cur = current();
    if std::env::var_os("ASTER_FORY_GOLDEN_WRITE").is_some() {
        let mut out = String::from(
            "# Fory XLANG encodings captured under fory-core =1.3.0 (xlang, compatible).\n",
        );
        for (n, b) in &cur {
            out.push_str(&format!("{n} {}\n", hex::encode(b)));
        }
        std::fs::create_dir_all(fixture_path().parent().unwrap()).unwrap();
        std::fs::write(fixture_path(), out).unwrap();
        return;
    }
    let golden = load();
    let mut changed = Vec::new();
    for (n, b) in &cur {
        let g = golden
            .get(*n)
            .unwrap_or_else(|| panic!("missing fixture {n}"));
        if g != b {
            changed.push(format!(
                "{n}: 1.3.0 {} bytes {}\n   now  {} bytes {}",
                g.len(),
                hex::encode(g),
                b.len(),
                hex::encode(b)
            ));
        }
    }
    assert!(
        changed.is_empty(),
        "Fory encodings changed vs 1.3.0:\n{}",
        changed.join("\n")
    );
}

#[test]
fn golden_1_3_0_bytes_still_decode() {
    let g = load();
    let mut failures = Vec::new();
    let mut check = |name: &str, ok: Result<bool, String>| match ok {
        Ok(true) => {}
        Ok(false) => failures.push(format!("{name}: decoded to a different value")),
        Err(e) => failures.push(format!("{name}: decode error: {e}")),
    };

    check(
        "namespace.read",
        CoreNamespaceCapability::decode_fory(&g["namespace.read"])
            .map(|c| c == CoreNamespaceCapability::Read([0x11; 32]))
            .map_err(|e| e.to_string()),
    );
    check(
        "namespace.write",
        CoreNamespaceCapability::decode_fory(&g["namespace.write"])
            .map(|c| c == CoreNamespaceCapability::Write([0x22; 32]))
            .map_err(|e| e.to_string()),
    );
    check(
        "hpke.envelope",
        HpkeEnvelope::decode_fory(&g["hpke.envelope"])
            .map(|e| e == hpke())
            .map_err(|e| e.to_string()),
    );
    check(
        "grant.active",
        GrantRecord::decode_fory(&g["grant.active"])
            .map(|r| r == GrantRecord::active("write", 1, g["hpke.envelope"].clone()))
            .map_err(|e| e.to_string()),
    );
    check(
        "grant.revoked",
        GrantRecord::decode_fory(&g["grant.revoked"])
            .map(|r| r == GrantRecord::revoked("read", "rotated"))
            .map_err(|e| e.to_string()),
    );
    check(
        "topo.position",
        decode_position(&g["topo.position"])
            .map(|p| p == position())
            .map_err(|e| e.to_string()),
    );
    check(
        "topo.rtt",
        decode_rtt_edge(&g["topo.rtt"])
            .map(|p| p == rtt())
            .map_err(|e| e.to_string()),
    );
    check(
        "topo.lan",
        decode_lan_edge(&g["topo.lan"])
            .map(|p| p == lan())
            .map_err(|e| e.to_string()),
    );
    assert!(
        failures.is_empty(),
        "1.3.0 bytes no longer decode:\n{}",
        failures.join("\n")
    );
}
