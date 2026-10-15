//! One integration-test binary for the `aster` facade.
//!
//! Each file in this directory used to be its own test target. Every target
//! is a separate crate that recompiles the generic code it instantiates from
//! `aster`, tokio and iroh, and a separate link of a large debug binary, so
//! CI spent most of the Rust test time compiling and linking rather than
//! testing. The files are unchanged modules here, each keeping its own
//! `#![cfg(feature = ...)]` gate, so a run selects tests exactly as before:
//! `cargo test -p aster --features rpc --test it rpc_client::` runs what
//! `--test rpc_client` used to.
//!
//! Some targets stay separate in `aster/tests/`, because sharing a process
//! would change what they prove:
//!
//! - `otel.rs`, `health.rs`: process-global OTel pipeline and `tracing`
//!   subscriber; cases re-run themselves in child processes.
//! - `rpc_corrupt_frames.rs`: installs a process-wide `tracing` subscriber.
//! - `rpc_index_collision.rs`: relies on being the only crate allocating Fory
//!   type indices from 0 (a per-crate counter in `fory-derive`).
//! - `connection_lifecycle.rs`: needs `reactor-test-hooks`, a separate build.
//! - `coordination_*.rs`, `enrollment*.rs`: built under their own feature
//!   sets (`--no-default-features --features coordination`, `enrollment,docs`,
//!   ...), where this binary's other modules must not join the run; CI
//!   discovers the coordination targets by their `#![cfg]` line.
//!
//! A file added here must also be declared below; `every_file_is_a_module`
//! fails otherwise, since an undeclared file would silently never compile.

mod admission;
mod authority_report;
mod binding_support;
mod blob_compression;
mod blob_fetch_multi;
mod blob_gc;
mod connection;
mod contract_defaults_vector;
mod durable_docs;
mod dynamic_dispatch;
mod dynamic_payload_vector;
mod dynamic_service_vector;
mod expose;
mod expose_foreign;
mod facade;
mod foreign_auth;
mod frame_limit_resolver;
mod grant_vectors;
mod grants;
mod hpke_identity;
mod identity;
mod lease;
mod namespace;
mod native_interop;
mod no_fory_attestation;
mod peer_provenance;
mod relay_smoke;
mod rpc_auth;
mod rpc_baseline;
mod rpc_bytes_wire;
mod rpc_client;
mod rpc_contract;
mod rpc_deadline;
mod rpc_dynamic_proxy;
mod rpc_error_mapping;
mod rpc_expose_coexist;
mod rpc_interceptor;
mod rpc_lease;
mod rpc_nested_payload;
mod rpc_no_request;
mod rpc_publish;
mod rpc_roundtrip;
mod rpc_service;
mod rpc_service_streaming;
mod rpc_sessions;
mod rpc_shell;
mod rpc_surface;
mod rpc_topology;
mod storage_perms;
mod ticket;
mod topology_locality;
mod topology_shared;
mod upcall_vectors;
mod upcalls;
mod write_authority;

// The interop matrix's `InteropEcho` contract, shared by the dynamic payload
// and service vector modules (one copy per crate: clippy::duplicate_mod).
#[cfg(feature = "rpc")]
#[path = "../../../tests/interop/rust/contract.rs"]
mod interop;

/// Every `.rs` file beside this one is declared as a module above. Cargo does
/// not discover files in a test target's subdirectory, so a file that is not
/// declared is never compiled and its tests never run.
#[test]
fn every_file_is_a_module() {
    let declared: std::collections::BTreeSet<&str> = include_str!("main.rs")
        .lines()
        .filter_map(|l| l.strip_prefix("mod ")?.strip_suffix(';'))
        .collect();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read tests/it") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
            if stem != "main" && !declared.contains(stem.as_str()) {
                missing.push(stem);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "tests/it files not declared in main.rs: {missing:?}"
    );
}
