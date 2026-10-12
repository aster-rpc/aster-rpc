//! The canonical manifest form against Python's own. Fixtures: the
//! `InteropEcho` and `InteropSession` manifests from `aster contract gen`
//! (Python CLI), with a non-ASCII description and tags added; `*.canonical`
//! is `ContractManifest.from_json(json.dumps(m)).to_json(indent=None)`.

use aster_directory_client::manifest::{canonical, canonical_all};

const FILE: &str = include_str!("fixtures/interop.json");

#[test]
fn matches_python_canonical_json_and_contract_id() {
    let all = canonical_all(FILE).unwrap();
    let expected = [
        (
            "InteropEcho",
            include_str!("fixtures/InteropEcho.canonical"),
            "da836a1f2aff2d7709166846407f07e855fe48faa18e96795db252249a822790",
        ),
        (
            "InteropSession",
            include_str!("fixtures/InteropSession.canonical"),
            "879de985e35bda67bdb1ae5a81fc034582b861b732b6fc39c1c609534ff902cd",
        ),
    ];
    assert_eq!(all.len(), 2);
    for (m, (service, json, id)) in all.iter().zip(expected) {
        assert_eq!(m.service, service);
        assert_eq!(m.json, json);
        assert_eq!(m.contract_id, id);
    }
}

#[test]
fn rejects_unknown_fields_and_unversioned_manifests() {
    assert!(canonical(r#"{"v": 1, "service": "X", "extra": 1}"#).is_err());
    assert!(canonical(r#"{"service": "X"}"#).is_err());
}

#[test]
fn fills_defaults_in_field_order() {
    let m = canonical(r#"{"service": "X", "v": 1, "version": "2"}"#).unwrap();
    assert!(
        m.json
            .starts_with(r#"{"v": 1, "service": "X", "version": 2, "contract_id": ""#),
        "{}",
        m.json
    );
    assert!(
        m.json
            .ends_with(r#""published_by": "", "published_at_epoch_ms": 0}"#),
        "{}",
        m.json
    );
}
