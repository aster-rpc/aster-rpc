#![cfg(feature = "rpc")]
//! `conformance/vectors/dynamic-service.json`: `DynamicService` registration
//! JSON → expected `contract_id` (design
//! `docs/_internal/bindings/dynamic-dispatch-design.md` §1.4). Every binding's
//! decorator lowering asserts against it.
//!
//! Each case's expected id is cross-checked against an independent source
//! where one exists: the golden vectors in `contract-identity.json` and
//! `contract-defaults.json`, the `tests/interop` `InteropEcho` contract (whose
//! id the 4x4 interop matrix pins), and `#[aster::service]` equivalents of the
//! mission-control services.
//!
//! Regenerate (only for an intended, recorded change):
//! `ASTER_VECTORS_WRITE=1 cargo test -p aster --features rpc --test it dynamic_service_vector::`

use std::collections::BTreeMap;
use std::path::PathBuf;

use aster::rpc::{
    async_trait, contract_id, require_all_of, require_any_of, require_role, AsterType,
    CapabilityKind, CapabilityRequirement, DynamicService, MethodDesc, MethodPattern,
    RequestStream, ResponseSink, ScopeKind, SerializationMode, ServiceContract, TypeDef, TypeRef,
};
use aster_transport_core::contract::{canonical_xlang_bytes_service_contract, compute_contract_id};
use fory_derive::ForyStruct;
use serde_json::{json, Value};

use crate::interop;

/// The `InteropEcho` contract id the interop servers report
/// (`interop_echo contract`; `tests/interop/run.py` fails the matrix when the
/// languages disagree).
const INTEROP_ECHO_ID: &str = "5dafded4d926cd6c1dfe4006f994ed240b8d37b7f73f6789bf5288da0170fc16";

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../conformance/vectors")
}

fn read_json(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(vectors_dir().join(name)).unwrap()).unwrap()
}

// ── mission-control types (fields identical to examples/rust/mission-control) ──

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/StatusRequest")]
struct StatusRequest {
    agent_id: String,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/StatusResponse")]
struct StatusResponse {
    agent_id: String,
    status: String,
    uptime_secs: i64,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/LogEntry")]
struct LogEntry {
    timestamp: f64,
    level: String,
    message: String,
    agent_id: String,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/SubmitLogResult")]
struct SubmitLogResult {
    accepted: bool,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/TailRequest")]
struct TailRequest {
    agent_id: String,
    level: String,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/MetricPoint")]
struct MetricPoint {
    name: String,
    value: f64,
    timestamp: f64,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/IngestResult")]
struct IngestResult {
    accepted: i32,
    dropped: i32,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/Command")]
struct Command {
    command: String,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/CommandResult")]
struct CommandResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Heartbeat with a nested list of capabilities, so the session case
/// publishes a type graph deeper than one level.
#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/Capability")]
struct Capability {
    name: String,
    #[aster(default = 1)]
    level: i32,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/Heartbeat")]
struct Heartbeat {
    agent_id: String,
    capabilities: Vec<Capability>,
    #[aster(default)]
    labels: BTreeMap<String, String>,
}

#[allow(dead_code)]
#[derive(ForyStruct, AsterType, Default)]
#[aster(wire = "mission/Assignment")]
struct Assignment {
    task_id: String,
    command: Option<String>,
}

#[aster::service(name = "MissionControl", version = 1)]
trait MissionControl {
    #[rpc(name = "getStatus")]
    async fn get_status(&self, req: StatusRequest) -> aster::Result<StatusResponse>;
    #[rpc(name = "submitLog")]
    async fn submit_log(&self, entry: LogEntry) -> aster::Result<SubmitLogResult>;
    #[rpc(server_stream, name = "tailLogs")]
    async fn tail_logs(&self, req: TailRequest, out: ResponseSink<LogEntry>) -> aster::Result<()>;
    #[rpc(client_stream, name = "ingestMetrics")]
    async fn ingest_metrics(&self, reqs: RequestStream<MetricPoint>)
        -> aster::Result<IngestResult>;
    #[rpc(bidi_stream, name = "runCommand")]
    async fn run_command(
        &self,
        reqs: RequestStream<Command>,
        out: ResponseSink<CommandResult>,
    ) -> aster::Result<()>;
}

/// `examples/python/mission_control/services_auth.py`: per-method `requires`.
#[aster::service(name = "MissionControl", version = 2)]
trait MissionControlAuth {
    #[rpc(name = "getStatus", idempotent, requires = require_role("status"))]
    async fn get_status(&self, req: StatusRequest) -> aster::Result<StatusResponse>;
    #[rpc(name = "submitLog")]
    async fn submit_log(&self, entry: LogEntry) -> aster::Result<SubmitLogResult>;
    #[rpc(server_stream, name = "tailLogs", requires = require_any_of(["logs", "admin"]))]
    async fn tail_logs(&self, req: TailRequest, out: ResponseSink<LogEntry>) -> aster::Result<()>;
    #[rpc(client_stream, name = "ingestMetrics", requires = require_role("ingest"))]
    async fn ingest_metrics(&self, reqs: RequestStream<MetricPoint>)
        -> aster::Result<IngestResult>;
}

// Only the generated `contract()` is used; these impls just satisfy the bound.
struct Nop;

#[async_trait]
impl MissionControl for Nop {
    async fn get_status(&self, _: StatusRequest) -> aster::Result<StatusResponse> {
        unimplemented!()
    }
    async fn submit_log(&self, _: LogEntry) -> aster::Result<SubmitLogResult> {
        unimplemented!()
    }
    async fn tail_logs(&self, _: TailRequest, _: ResponseSink<LogEntry>) -> aster::Result<()> {
        unimplemented!()
    }
    async fn ingest_metrics(&self, _: RequestStream<MetricPoint>) -> aster::Result<IngestResult> {
        unimplemented!()
    }
    async fn run_command(
        &self,
        _: RequestStream<Command>,
        _: ResponseSink<CommandResult>,
    ) -> aster::Result<()> {
        unimplemented!()
    }
}

#[async_trait]
impl MissionControlAuth for Nop {
    async fn get_status(&self, _: StatusRequest) -> aster::Result<StatusResponse> {
        unimplemented!()
    }
    async fn submit_log(&self, _: LogEntry) -> aster::Result<SubmitLogResult> {
        unimplemented!()
    }
    async fn tail_logs(&self, _: TailRequest, _: ResponseSink<LogEntry>) -> aster::Result<()> {
        unimplemented!()
    }
    async fn ingest_metrics(&self, _: RequestStream<MetricPoint>) -> aster::Result<IngestResult> {
        unimplemented!()
    }
}

// ── contract-defaults.json's Settings ──

#[allow(dead_code)]
#[derive(ForyStruct, AsterType)]
#[aster(wire = "defaults.v1/Settings")]
struct Settings {
    name: String,
    #[aster(default = "idle")]
    state: String,
    #[aster(default = "")]
    note: String,
    #[aster(default = true)]
    enabled: bool,
    #[aster(default = -3)]
    retries: i32,
    #[aster(default = 1_700_000_000_000i64)]
    since_ms: i64,
    #[aster(default = 0.5)]
    ratio: f64,
    #[aster(default = "x")]
    label: Option<String>,
    #[aster(default)]
    tags: Vec<String>,
    #[aster(default)]
    attrs: BTreeMap<String, String>,
    #[fory(bytes)]
    blob: Vec<u8>,
}

// ── building the cases ──

fn method<Req: AsterType, Resp: AsterType>(name: &str, pattern: MethodPattern) -> MethodDesc {
    MethodDesc::new(name, pattern, TypeRef::of::<Req>(), TypeRef::of::<Resp>())
}

fn defs(list: &[TypeDef]) -> Vec<TypeDef> {
    list.to_vec()
}

/// `contract-identity.json` golden vectors, lowered to `DynamicService`. That
/// file spells enums as integers (Python reference output, 2026-04-07).
///
/// Its `expected_canonical_hex` / `expected_blake3` predate canonical field 7
/// (`producer_language`, §11.3.2.3), which every current encoder appends as a
/// string -- `0x02` when empty. So the third element is the vector's canonical
/// hex with that byte appended, and the expected id is BLAKE3 of those bytes,
/// not the file's stale `expected_blake3`.
fn identity_cases() -> Vec<(String, DynamicService, String)> {
    let doc = read_json("contract-identity.json");
    doc["golden_vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            let c = &v["contract"];
            let mut svc = DynamicService::new(
                c["name"].as_str().unwrap(),
                c["version"].as_i64().unwrap() as i32,
            );
            svc.scope = match c["scoped"].as_u64().unwrap() {
                0 => ScopeKind::Shared,
                _ => ScopeKind::Session,
            };
            svc.serialization_modes = vec![];
            assert!(c["serialization_modes"].as_array().unwrap().is_empty());
            assert!(c["requires"].is_null());
            for m in c["methods"].as_array().unwrap() {
                let pattern = match m["pattern"].as_u64().unwrap() {
                    0 => MethodPattern::Unary,
                    1 => MethodPattern::ServerStream,
                    2 => MethodPattern::ClientStream,
                    _ => MethodPattern::BidiStream,
                };
                let mut md = MethodDesc::new(
                    m["name"].as_str().unwrap(),
                    pattern,
                    TypeRef::new(m["request_type"].as_str().unwrap()),
                    TypeRef::new(m["response_type"].as_str().unwrap()),
                );
                md.idempotent = m["idempotent"].as_bool().unwrap();
                let t = m["default_timeout"].as_f64().unwrap();
                md.default_timeout = (t != 0.0).then_some(t);
                if !m["requires"].is_null() {
                    let r = &m["requires"];
                    md.requires = Some(CapabilityRequirement {
                        kind: match r["kind"].as_u64().unwrap() {
                            0 => CapabilityKind::Role,
                            1 => CapabilityKind::AnyOf,
                            _ => CapabilityKind::AllOf,
                        },
                        roles: r["roles"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|s| s.as_str().unwrap().to_string())
                            .collect(),
                    });
                }
                svc.methods.push(md);
            }
            let name = format!("contract-identity: {}", v["name"].as_str().unwrap());
            let canonical = format!("{}02", v["expected_canonical_hex"].as_str().unwrap());
            (name, svc, canonical)
        })
        .collect()
}

fn defaults_case() -> DynamicService {
    let mut svc = DynamicService::new("Defaults", 1);
    svc.methods
        .push(method::<Settings, Settings>("put", MethodPattern::Unary));
    svc.type_defs = defs(&[Settings::aster_type_def()]);
    svc
}

fn interop_echo_case() -> DynamicService {
    use interop::{EchoReply, EchoRequest};
    let mut svc = DynamicService::new("InteropEcho", 1);
    svc.methods = vec![
        method::<EchoRequest, EchoReply>("echo", MethodPattern::Unary),
        method::<EchoRequest, EchoReply>("generate", MethodPattern::ServerStream),
        method::<EchoRequest, EchoReply>("ingest", MethodPattern::ClientStream),
        method::<EchoRequest, EchoReply>("chat", MethodPattern::BidiStream),
        method::<EchoRequest, EchoReply>("hold", MethodPattern::Unary),
        method::<EchoRequest, EchoReply>("cancelled", MethodPattern::Unary),
        method::<EchoRequest, EchoReply>("traceEcho", MethodPattern::Unary),
    ];
    svc.type_defs = defs(&[EchoRequest::aster_type_def(), EchoReply::aster_type_def()]);
    svc
}

fn mission_control_case() -> DynamicService {
    let mut svc = DynamicService::new("MissionControl", 1);
    svc.methods = vec![
        method::<StatusRequest, StatusResponse>("getStatus", MethodPattern::Unary),
        method::<LogEntry, SubmitLogResult>("submitLog", MethodPattern::Unary),
        method::<TailRequest, LogEntry>("tailLogs", MethodPattern::ServerStream),
        method::<MetricPoint, IngestResult>("ingestMetrics", MethodPattern::ClientStream),
        method::<Command, CommandResult>("runCommand", MethodPattern::BidiStream),
    ];
    svc.type_defs = defs(&[
        StatusRequest::aster_type_def(),
        StatusResponse::aster_type_def(),
        LogEntry::aster_type_def(),
        SubmitLogResult::aster_type_def(),
        TailRequest::aster_type_def(),
        MetricPoint::aster_type_def(),
        IngestResult::aster_type_def(),
        Command::aster_type_def(),
        CommandResult::aster_type_def(),
    ]);
    svc
}

fn mission_control_auth_case() -> DynamicService {
    let mut svc = DynamicService::new("MissionControl", 2);
    let mut get = method::<StatusRequest, StatusResponse>("getStatus", MethodPattern::Unary);
    get.idempotent = true;
    get.requires = Some(require_role("status"));
    let mut tail = method::<TailRequest, LogEntry>("tailLogs", MethodPattern::ServerStream);
    tail.requires = Some(require_any_of(["logs", "admin"]));
    let mut ingest =
        method::<MetricPoint, IngestResult>("ingestMetrics", MethodPattern::ClientStream);
    ingest.requires = Some(require_role("ingest"));
    svc.methods = vec![
        get,
        method::<LogEntry, SubmitLogResult>("submitLog", MethodPattern::Unary),
        tail,
        ingest,
    ];
    svc
}

/// `examples/python/mission_control/services.py`'s session-scoped
/// `AgentSession`, Rust-typed, with a service-wide requirement and timeouts.
fn agent_session_case() -> DynamicService {
    let mut svc = DynamicService::new("AgentSession", 1);
    svc.scope = ScopeKind::Session;
    svc.requires = Some(require_any_of(["agent", "admin"]));
    let mut register = method::<Heartbeat, Assignment>("register", MethodPattern::Unary);
    register.default_timeout = Some(30.0);
    let mut heartbeat = method::<Heartbeat, Assignment>("heartbeat", MethodPattern::Unary);
    heartbeat.idempotent = true;
    heartbeat.default_timeout = Some(2.5);
    let mut run = method::<Command, CommandResult>("runCommand", MethodPattern::BidiStream);
    run.requires = Some(require_role("admin"));
    svc.methods = vec![register, heartbeat, run];
    svc.type_defs = defs(&[
        Heartbeat::aster_type_def(),
        Capability::aster_type_def(),
        Assignment::aster_type_def(),
        Command::aster_type_def(),
        CommandResult::aster_type_def(),
    ]);
    svc
}

/// Everything that is *not* part of the contract (limits, `iroh_only`,
/// display names, method order) set, on top of an all-of service requirement
/// and native mode.
fn kitchen_sink_case() -> DynamicService {
    let mut svc = DynamicService::new("ops.Fleet", 3);
    svc.serialization_modes = vec![SerializationMode::Xlang, SerializationMode::Native];
    svc.producer_language = "typescript".into();
    svc.requires = Some(require_all_of(["ops", "oncall"]));
    svc.iroh_only = true;
    let mut drain = method::<StatusRequest, StatusResponse>("drain", MethodPattern::Unary);
    drain.default_timeout = Some(0.25);
    drain.limits.max_frame_size = Some(64 * 1024);
    let upload = method::<MetricPoint, IngestResult>("upload", MethodPattern::ClientStream);
    svc.methods = vec![upload, drain];
    svc
}

fn valid_cases() -> Vec<(String, String, DynamicService, String)> {
    let mut out: Vec<(String, String, DynamicService, String)> = identity_cases()
        .into_iter()
        .map(|(name, svc, canonical)| {
            let id = compute_contract_id(&hex::decode(canonical).unwrap());
            let source = "contract-identity.json (+ empty producer_language)".to_string();
            (name, source, svc, id)
        })
        .collect();
    let mut add = |name: &str, source: &str, svc: DynamicService| {
        let id = svc.contract_id().unwrap();
        out.push((name.to_string(), source.to_string(), svc, id));
    };
    add(
        "contract-defaults: Defaults v1",
        "contract-defaults.json",
        defaults_case(),
    );
    add(
        "interop: InteropEcho v1",
        "tests/interop/rust/contract.rs",
        interop_echo_case(),
    );
    add(
        "mission-control: MissionControl v1",
        "examples/rust/mission-control (#[aster::service] equivalent)",
        mission_control_case(),
    );
    add(
        "mission-control: MissionControl v2 with per-method requires",
        "examples/python/mission_control/services_auth.py shape (#[aster::service] equivalent)",
        mission_control_auth_case(),
    );
    add(
        "mission-control: AgentSession v1 (session scope, service requires, timeouts)",
        "examples/python/mission_control/services.py shape",
        agent_session_case(),
    );
    add(
        "non-contract fields and native mode",
        "synthetic",
        kitchen_sink_case(),
    );
    out
}

/// Services every binding must refuse, by `ValidationError::code()`. `schema`
/// means the JSON does not parse as a `DynamicService` at all.
fn invalid_cases() -> Vec<(&'static str, Value, &'static str)> {
    let base = || serde_json::to_value(interop_echo_case()).unwrap();
    let mut out = Vec::new();
    let mut case = |name: &'static str, code: &'static str, f: &dyn Fn(&mut Value)| {
        let mut v = base();
        f(&mut v);
        out.push((name, v, code));
    };
    case("unknown key", "schema", &|v| v["requiers"] = json!(null));
    case("unknown serialization mode", "schema", &|v| {
        v["serialization_modes"] = json!(["json"])
    });
    case("reserved namespace", "reserved_namespace", &|v| {
        v["name"] = json!("aster.ops.Fleet")
    });
    case("service name not NFC", "not_normalized", &|v| {
        v["name"] = json!("Cafe\u{301}")
    });
    case("duplicate method name", "duplicate_method", &|v| {
        v["methods"][1]["name"] = json!("echo")
    });
    case("uppercase hash", "malformed_hash", &|v| {
        v["methods"][0]["request"]["hash"] = json!("AB".repeat(32));
    });
    case("short hash", "malformed_hash", &|v| {
        v["methods"][0]["response"]["hash"] = json!("00")
    });
    case("negative timeout", "bad_timeout", &|v| {
        v["methods"][0]["default_timeout"] = json!(-1.0)
    });
    case(
        "frame limit above MAX_FRAME_SIZE",
        "bad_frame_limit",
        &|v| v["methods"][0]["limits"] = json!({"max_frame_size": 16 * 1024 * 1024 + 1}),
    );
    case(
        "native without producer_language",
        "producer_language",
        &|v| v["serialization_modes"] = json!(["native"]),
    );
    case(
        "producer_language without native",
        "producer_language",
        &|v| v["producer_language"] = json!("python"),
    );
    case(
        "type_def not reachable from any method",
        "unreachable_type_def",
        &|v| {
            v["type_defs"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::to_value(Command::aster_type_def()).unwrap())
        },
    );
    out
}

fn actual() -> Value {
    let cases: Vec<Value> = valid_cases()
        .into_iter()
        .map(|(name, source, svc, id)| {
            json!({
                "name": name,
                "source": source,
                "service": serde_json::to_value(&svc).unwrap(),
                "expected_contract_id": id,
            })
        })
        .collect();
    let invalid: Vec<Value> = invalid_cases()
        .into_iter()
        .map(|(name, service, code)| json!({"name": name, "service": service, "error": code}))
        .collect();
    json!({
        "description": "DynamicService registration JSON (dynamic-dispatch-design.md §1.5) -> expected contract_id. contract_id = BLAKE3(canonical_xlang_bytes(ServiceContract)), with the contract derived from the service (§1.2). Every binding's decorator lowering must produce a service with the same contract_id; `invalid` services must be refused with the given error code (`schema` = does not parse).",
        "rust_source": "aster/tests/it/dynamic_service_vector.rs",
        "cases": cases,
        "invalid": invalid,
    })
}

fn path() -> PathBuf {
    vectors_dir().join("dynamic-service.json")
}

#[test]
fn dynamic_service_vector() {
    let got = actual();
    if std::env::var_os("ASTER_VECTORS_WRITE").is_some() {
        std::fs::write(path(), serde_json::to_string_pretty(&got).unwrap() + "\n").unwrap();
    }
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(doc, got, "dynamic-service vector drifted");

    // What a binding does: parse each case's JSON, then compute the id.
    for case in doc["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let svc = DynamicService::from_json(&case["service"].to_string())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            svc.contract_id().unwrap(),
            case["expected_contract_id"].as_str().unwrap(),
            "{name}"
        );
        // The core subset collection publishes under the same id.
        let entries = svc.contract_collection("vector", 0).unwrap();
        assert_eq!(
            entries.len(),
            2 + svc.type_defs.len(),
            "{name}: contract.bin + types + manifest.json"
        );
    }
    for case in doc["invalid"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let code = match serde_json::from_value::<DynamicService>(case["service"].clone()) {
            Err(_) => "schema",
            Ok(svc) => svc
                .validate()
                .map(|()| "valid")
                .unwrap_or_else(|e| e.code()),
        };
        assert_eq!(code, case["error"].as_str().unwrap(), "{name}");
    }
}

#[test]
fn identity_cases_match_contract_identity_json() {
    // The dynamic path reproduces contract-identity.json's canonical bytes
    // (plus the empty producer_language; see `identity_cases`).
    for (name, svc, canonical) in identity_cases() {
        assert_eq!(
            hex::encode(canonical_xlang_bytes_service_contract(&svc.contract())),
            canonical,
            "{name}"
        );
        assert_eq!(
            svc.contract_id().unwrap(),
            compute_contract_id(&hex::decode(&canonical).unwrap()),
            "{name}"
        );
    }
}

#[test]
fn defaults_case_matches_contract_defaults_json() {
    let doc = read_json("contract-defaults.json");
    assert_eq!(
        defaults_case().contract_id().unwrap(),
        doc["vector"]["expected_contract_id"].as_str().unwrap()
    );
}

/// The dynamic service and the `#[aster::service]` it mirrors commit to the
/// same canonical contract bytes.
fn assert_same_contract(name: &str, dynamic: &DynamicService, generated: &ServiceContract) {
    assert_eq!(
        canonical_xlang_bytes_service_contract(&dynamic.contract()),
        canonical_xlang_bytes_service_contract(generated),
        "{name}: canonical bytes"
    );
    assert_eq!(
        dynamic.contract_id().unwrap(),
        contract_id(generated),
        "{name}"
    );
}

#[test]
fn interop_echo_matches_macro_and_pinned_id() {
    let svc = interop_echo_case();
    assert_same_contract(
        "InteropEcho",
        &svc,
        &interop::InteropEchoServer::<interop::EchoImpl>::contract(),
    );
    assert_eq!(svc.contract_id().unwrap(), INTEROP_ECHO_ID);
}

#[test]
fn mission_control_matches_macro() {
    assert_same_contract(
        "MissionControl",
        &mission_control_case(),
        &MissionControlServer::<Nop>::contract(),
    );
    assert_same_contract(
        "MissionControl (auth)",
        &mission_control_auth_case(),
        &MissionControlAuthServer::<Nop>::contract(),
    );
}

#[test]
fn non_contract_fields_do_not_change_the_id() {
    let full = kitchen_sink_case();
    let mut bare = full.clone();
    bare.iroh_only = false;
    bare.methods.reverse();
    for m in &mut bare.methods {
        m.limits = Default::default();
        m.request.display.clear();
        m.response.display.clear();
    }
    assert_ne!(full, bare);
    assert_eq!(full.contract_id().unwrap(), bare.contract_id().unwrap());

    // ...while a contract field does.
    let mut changed = full.clone();
    changed.methods[1].default_timeout = Some(0.5);
    assert_ne!(full.contract_id().unwrap(), changed.contract_id().unwrap());
}
