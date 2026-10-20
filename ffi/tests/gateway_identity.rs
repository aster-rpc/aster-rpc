//! ABI 7.7: gateway identity through the C ABI, on the config-driven path
//! (`docs/_internal/web_gateway/gateway-identity.md`, slice 5). An enrolled
//! server takes `[identity]` from `aster_config_set_identity`; an enrolled
//! edge mints a user's token (`aster_enrollment_mint_user_token`) and sends
//! it as the `aster-on-behalf-of` metadata entry; the server's foreign
//! handler sees the forwarded identity (`ASTER_DISPATCH_FLAG_ON_BEHALF_OF`,
//! `aster_dispatch_identity` actor, `aster_dispatch_on_behalf_of` amr). The
//! edge, holding `aster.config-admin`, changes the server through
//! `aster.ops.NodeConfig` (`aster_node_config_call`), which handlers see
//! through `aster_server_node_properties`. The root is played with the
//! low-level Rust API. Loopback only.

use std::collections::{BTreeMap, BTreeSet};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aster::enrollment::{
    issue_credential, AdmissionClaims, EnrollmentIssueRequest, JwtEnrollmentIssuer, TypedAttribute,
};
use aster::rpc::{require_role, DynamicService, MethodDesc, MethodPattern, TypeRef};
use aster::{AsterConfig, Doc, Node, RelayMode, SecretKey};
use aster_transport_ffi::call::aster_call_buffer_release;
use aster_transport_ffi::config_identity::*;
use aster_transport_ffi::enrollment::*;
use aster_transport_ffi::identity::*;
use aster_transport_ffi::node::*;
use aster_transport_ffi::rpc::*;
use aster_transport_ffi::server::*;
use aster_transport_ffi::*;
use serde_json::{json, Value};

mod common;

const FLEET: &str = "ffi-gateway";
const OK: i32 = aster_status_t::ASTER_STATUS_OK as i32;
const INVALID: i32 = aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32;
const SERVER: u32 = aster_enrollment_side_t::ASTER_ENROLLMENT_SIDE_SERVER as u32;
const CLIENT: u32 = aster_enrollment_side_t::ASTER_ENROLLMENT_SIDE_CLIENT as u32;
const PERMISSION_DENIED: i32 = 7;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn key(seed: u8) -> SecretKey {
    SecretKey::from_bytes([seed; 32])
}
fn public(seed: u8) -> [u8; 32] {
    JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
}
fn id(seed: u8) -> String {
    hex::encode(public(seed))
}
fn bytes(b: &[u8]) -> aster_bytes_t {
    aster_bytes_t {
        ptr: if b.is_empty() {
            ptr::null()
        } else {
            b.as_ptr()
        },
        len: b.len(),
    }
}
unsafe fn copy(p: *const u8, n: u32) -> Vec<u8> {
    if p.is_null() || n == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(p, n as usize).to_vec()
    }
}

fn kv(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = (pairs.len() as u32).to_le_bytes().to_vec();
    for (k, v) in pairs {
        for part in [k, v] {
            out.extend((part.len() as u32).to_le_bytes());
            out.extend(part.as_bytes());
        }
    }
    out
}

fn decode_kv(b: &[u8]) -> Vec<(String, String)> {
    if b.is_empty() {
        return Vec::new();
    }
    let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
    let mut o = 4;
    let mut out = Vec::new();
    for _ in 0..u32_at(0) {
        let kl = u32_at(o);
        let k = String::from_utf8(b[o + 4..o + 4 + kl].to_vec()).unwrap();
        o += 4 + kl;
        let vl = u32_at(o);
        let v = String::from_utf8(b[o + 4..o + 4 + vl].to_vec()).unwrap();
        o += 4 + vl;
        out.push((k, v));
    }
    out
}

/// A root-issued credential for `seed` with `roles` and an issuer delegation.
fn credential(seed: u8, roles: Option<&str>, issuable: Option<&str>) -> String {
    let mut attributes = BTreeMap::new();
    if let Some(roles) = roles {
        attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
    }
    if let Some(issuable) = issuable {
        attributes.insert(
            "aster.identity.issuable_roles".into(),
            TypedAttribute::String(issuable.into()),
        );
    }
    let mut request = EnrollmentIssueRequest::new(public(seed), FLEET, now() + 3600);
    request.claims = AdmissionClaims {
        scopes: Some(BTreeSet::new()),
        tags: Some(BTreeSet::new()),
        attributes: Some(attributes),
    };
    issue_credential(&key(1), &request, now())
        .unwrap()
        .into_chain()
}

/// The root's fleet namespace (nothing is published: the server holds it).
struct Root {
    tokio: tokio::runtime::Runtime,
    node: Node,
    fleet: Doc,
}

fn root() -> Root {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let (node, fleet) = tokio.block_on(async {
        let node = Node::start(
            AsterConfig::builder()
                .relay(RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(key(1))
                .build(),
        )
        .await
        .unwrap();
        let fleet = node.docs().create().await.unwrap();
        (node, fleet)
    });
    Root { tokio, node, fleet }
}

impl Root {
    fn namespace(&self) -> String {
        self.fleet.id().unwrap().to_string()
    }
}

fn trust(root: &Root, credential: &str, enrollment: Value) -> Value {
    let mut section = json!({"audience": FLEET, "fleet_namespace": root.namespace()});
    for (k, v) in enrollment.as_object().unwrap() {
        section[k] = v.clone();
    }
    json!({
        "root_pubkey": hex::encode(public(1)),
        "enrollment_credential_chain": credential,
        "enrollment": section,
    })
}

unsafe fn runtime() -> aster_runtime_t {
    let mut rt = 0;
    assert_eq!(
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut rt),
        OK
    );
    rt
}

fn last_error() -> String {
    let mut buf = vec![0u8; 4096];
    let len = unsafe { aster_last_error_message(buf.as_mut_ptr(), buf.len()) };
    String::from_utf8_lossy(&buf[..len.min(buf.len())]).into_owned()
}

fn identity_view() -> aster_identity_config_t {
    let mut v: aster_identity_config_t = unsafe { std::mem::zeroed() };
    v.struct_size = std::mem::size_of::<aster_identity_config_t>() as u32;
    v
}

/// A loopback node (seed, `aster/1`), enrolled from `trust` when given, with
/// `identity` beside its config.
unsafe fn start(
    rt: aster_runtime_t,
    seed: u8,
    trust: Option<&Value>,
    side: u32,
    identity: Option<&aster_identity_config_t>,
) -> Result<(u64, u64), (i32, String)> {
    let alpns = [bytes(b"aster/1")];
    let secret = [seed; 32];
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), OK);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = bytes(b"127.0.0.1:0");
    cfg.secret_key = bytes(&secret);
    cfg.alpns = aster_bytes_list_t {
        items: alpns.as_ptr(),
        len: alpns.len(),
    };
    if let Some(identity) = identity {
        assert_eq!(aster_config_set_identity(&mut cfg, identity), OK);
    }
    let mut op = 0;
    let status = match trust {
        Some(trust) => {
            let text = trust.to_string();
            aster_node_start_enrolled(rt, &cfg, bytes(text.as_bytes()), side, 0, &mut op)
        }
        None => aster_node_start(rt, &cfg, 0, 0, &mut op),
    };
    let message = last_error();
    assert_eq!(aster_config_release(&mut cfg), OK);
    if status != OK {
        return Err((status, message));
    }
    let event = common::wait_for_operation(rt, op);
    if event.kind == aster_event_kind_t::ASTER_EVENT_NODE_CREATED as u32 {
        return Ok((event.handle, event.related));
    }
    let message =
        String::from_utf8_lossy(&copy(event.data_ptr, event.data_len as u32)).into_owned();
    aster_buffer_release(rt, event.buffer);
    Err((event.status as i32, message))
}

unsafe fn addr_json(rt: aster_runtime_t, node: u64) -> (String, Vec<String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut buf = vec![0u8; 4096];
        let mut out: aster_node_addr_t = std::mem::zeroed();
        assert_eq!(
            aster_node_addr_info(rt, node, buf.as_mut_ptr(), buf.len(), &mut out),
            OK
        );
        let s = |b: &aster_bytes_t| {
            String::from_utf8(std::slice::from_raw_parts(b.ptr, b.len).to_vec()).unwrap()
        };
        let direct: Vec<String> = if out.direct_addresses.len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(out.direct_addresses.items, out.direct_addresses.len)
                .iter()
                .map(s)
                .collect()
        };
        if !direct.is_empty() || Instant::now() > deadline {
            return (s(&out.endpoint_id), direct);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

unsafe fn connect(rt: aster_runtime_t, client: u64, server: u64) -> u64 {
    let (id, direct) = addr_json(rt, server);
    let direct_b: Vec<aster_bytes_t> = direct.iter().map(|d| bytes(d.as_bytes())).collect();
    let addr = aster_node_addr_t {
        endpoint_id: bytes(id.as_bytes()),
        relay_url: bytes(&[]),
        direct_addresses: aster_bytes_list_t {
            items: direct_b.as_ptr(),
            len: direct_b.len(),
        },
    };
    let cfg = aster_connect_config_t {
        struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
        flags: 0,
        node_id: bytes(id.as_bytes()),
        alpn: bytes(b"aster/1"),
        addr: &addr,
    };
    let mut op = 0;
    assert_eq!(aster_connect(rt, client, &cfg, 0, &mut op), OK);
    common::wait_for_successful_operation(rt, op, aster_event_kind_t::ASTER_EVENT_CONNECTED).handle
}

unsafe fn enroll(rt: aster_runtime_t, enrollment: u64, server: u64) {
    let (id, direct) = addr_json(rt, server);
    let text = json!({"node_id": id, "direct_addresses": direct}).to_string();
    let mut op = 0;
    assert_eq!(
        aster_enrollment_enroll(rt, enrollment, bytes(text.as_bytes()), 0, &mut op),
        OK
    );
    let event = common::wait_for_operation(rt, op);
    let data = String::from_utf8_lossy(&copy(event.data_ptr, event.data_len as u32)).into_owned();
    aster_buffer_release(rt, event.buffer);
    assert_eq!(
        event.kind,
        aster_event_kind_t::ASTER_EVENT_ENROLLMENT_RESULT as u32,
        "{data}"
    );
}

fn notes_json() -> String {
    let hash = |b: u8| TypeRef::new(format!("{b:02x}").repeat(32));
    let method = |name: &str| MethodDesc::new(name, MethodPattern::Unary, hash(0xa1), hash(0xb2));
    let mut read = method("read");
    read.requires = Some(require_role("notes.read"));
    let mut svc = DynamicService::new("test.Notes", 1);
    svc.methods = vec![method("whoami"), read];
    svc.to_json().unwrap()
}

#[derive(Clone, Copy)]
struct Srv {
    rt: aster_runtime_t,
    server: u64,
}

/// Answer every call with what its handler context holds, as JSON.
fn serve(srv: Srv, stop: Arc<AtomicBool>) {
    let mut buf: Vec<aster_dispatch_event_t> = Vec::with_capacity(16);
    while !stop.load(Ordering::SeqCst) {
        let n = unsafe { aster_server_poll(srv.rt, srv.server, buf.as_mut_ptr(), 16, 20) };
        unsafe { buf.set_len(n as usize) };
        for raw in buf.drain(..) {
            unsafe {
                let peer = String::from_utf8(copy(raw.peer_ptr, raw.peer_len)).unwrap();
                aster_dispatch_buffer_release(srv.rt, srv.server, raw.buffer);
                if raw.kind != aster_dispatch_event_kind_t::ASTER_DISPATCH_EVENT_CALL as u8 {
                    continue;
                }
                let call = raw.call_id;
                let mut ident: aster_dispatch_identity_t = std::mem::zeroed();
                ident.struct_size = std::mem::size_of::<aster_dispatch_identity_t>() as u32;
                assert_eq!(
                    aster_dispatch_identity(srv.rt, srv.server, call, &mut ident),
                    OK
                );
                let actor = String::from_utf8(copy(ident.actor_ptr, ident.actor_len)).unwrap();
                aster_dispatch_buffer_release(srv.rt, srv.server, ident.buffer);
                let kv_of = |f: unsafe extern "C" fn(
                    aster_runtime_t,
                    aster_server_t,
                    u64,
                    *mut *const u8,
                    *mut u32,
                    *mut aster_buffer_t,
                ) -> i32| {
                    let (mut p, mut l, mut b) = (ptr::null(), 0u32, 0u64);
                    assert_eq!(f(srv.rt, srv.server, call, &mut p, &mut l, &mut b), OK);
                    let out = decode_kv(&copy(p, l));
                    aster_dispatch_buffer_release(srv.rt, srv.server, b);
                    out
                };
                let forwarded = kv_of(aster_dispatch_on_behalf_of);
                let attributes = kv_of(aster_dispatch_attributes);
                let mut props = bytes(b"");
                assert_eq!(aster_server_node_properties(srv.server, &mut props), OK);
                let props_json: Value =
                    serde_json::from_slice(&copy(props.ptr, props.len as u32)).unwrap();
                aster_string_release(props.ptr, props.len);
                let answer = json!({
                    "principal": peer,
                    "on_behalf_of": raw.flags & ASTER_DISPATCH_FLAG_ON_BEHALF_OF != 0,
                    "actor": actor,
                    "forwarded": forwarded,
                    "attributes": attributes,
                    "properties": props_json,
                })
                .to_string();
                let mut required = 0;
                aster_dispatch_respond(
                    srv.rt,
                    srv.server,
                    call,
                    answer.as_ptr(),
                    answer.len() as u32,
                    ptr::null(),
                    0,
                    &mut required,
                );
            }
        }
    }
}

/// A unary call: `Ok(json)` or `Err(code)`.
unsafe fn call(
    rt: aster_runtime_t,
    conn: u64,
    method: &str,
    token: Option<&str>,
) -> Result<Value, i32> {
    let metadata = match token {
        Some(token) => kv(&[("aster-on-behalf-of", token)]),
        None => Vec::new(),
    };
    let raw = aster_call_options_t {
        struct_size: std::mem::size_of::<aster_call_options_t>() as u32,
        version: 1,
        service: bytes(b"test.Notes"),
        method: bytes(method.as_bytes()),
        metadata: bytes(&metadata),
        session_id: 0,
        deadline_ms: 20_000,
        serialization_mode: 0,
        wire_call_id: 0,
    };
    let req = b"{}";
    let (mut rp, mut rl, mut rb) = (ptr::null(), 0u32, 0u64);
    let (mut code, mut sp, mut sl, mut sb) = (0i32, ptr::null(), 0u32, 0u64);
    assert_eq!(
        aster_rpc_unary(
            rt,
            conn,
            &raw,
            req.as_ptr(),
            req.len() as u32,
            &mut rp,
            &mut rl,
            &mut rb,
            &mut code,
            &mut sp,
            &mut sl,
            &mut sb,
        ),
        OK
    );
    let resp = copy(rp, rl);
    aster_call_buffer_release(rt, rb);
    aster_call_buffer_release(rt, sb);
    if code != 0 {
        return Err(code);
    }
    Ok(serde_json::from_slice(&resp).unwrap())
}

unsafe fn mint(rt: aster_runtime_t, edge: u64, request: Value) -> Result<String, String> {
    let text = request.to_string();
    let mut out = bytes(b"");
    match aster_enrollment_mint_user_token(rt, edge, bytes(text.as_bytes()), &mut out) {
        OK => {
            let token = String::from_utf8(copy(out.ptr, out.len as u32)).unwrap();
            assert_eq!(aster_string_release(out.ptr, out.len), OK);
            Ok(token)
        }
        status => {
            assert_eq!(status, INVALID);
            assert!(out.ptr.is_null());
            Err(last_error())
        }
    }
}

/// `aster_node_config_call`: the result payload `{"code", "result"|"message"}`.
unsafe fn node_config(rt: aster_runtime_t, conn: u64, method: u32, request: Value) -> Value {
    let text = request.to_string();
    let mut op = 0;
    assert_eq!(
        aster_node_config_call(rt, conn, method, bytes(text.as_bytes()), 0, &mut op),
        OK,
        "{}",
        last_error()
    );
    let event = common::wait_for_operation(rt, op);
    assert_eq!(
        event.kind,
        aster_event_kind_t::ASTER_EVENT_NODE_CONFIG_RESULT as u32
    );
    assert_eq!(event.handle, conn);
    let payload = copy(event.data_ptr, event.data_len as u32);
    aster_buffer_release(rt, event.buffer);
    serde_json::from_slice(&payload).unwrap()
}

fn entry<'a>(pairs: &'a [Value], key: &str) -> Vec<&'a str> {
    pairs
        .iter()
        .filter(|p| p[0] == key)
        .map(|p| p[1].as_str().unwrap())
        .collect()
}

const GET: u32 = aster_node_config_method_t::ASTER_NODE_CONFIG_GET as u32;
const SET: u32 = aster_node_config_method_t::ASTER_NODE_CONFIG_SET as u32;
const CLEAR: u32 = aster_node_config_method_t::ASTER_NODE_CONFIG_CLEAR as u32;
const ACK: u32 = aster_node_config_method_t::ASTER_NODE_CONFIG_ACK as u32;

#[test]
fn forwarded_identity_and_node_config_through_the_c_abi() {
    let root = root();
    let files = tempfile::tempdir().unwrap();
    unsafe {
        let rt = runtime();
        let mut identity = identity_view();
        identity.accept_on_behalf_of = 1;
        let (server_node, _server) = start(
            rt,
            2,
            Some(&trust(
                &root,
                &credential(2, None, None),
                json!({"state_path": files.path().join("server.json").to_str().unwrap()}),
            )),
            SERVER,
            Some(&identity),
        )
        .unwrap();
        let mut builder = 0;
        assert_eq!(aster_server_builder_new(rt, server_node, &mut builder), OK);
        let svc = notes_json();
        let mut idx = 0;
        assert_eq!(
            aster_server_add_service_json(builder, svc.as_ptr(), svc.len() as u32, &mut idx),
            OK
        );
        let mut srv = 0;
        assert_eq!(aster_server_start(rt, builder, 64, &mut srv), OK);
        let stop = Arc::new(AtomicBool::new(false));
        let poller = {
            let stop = stop.clone();
            let s = Srv { rt, server: srv };
            std::thread::spawn(move || serve(s, stop))
        };

        // The edge: a gateway that may issue notes.read, and a config admin.
        let (edge_node, edge) = start(
            rt,
            5,
            Some(&trust(
                &root,
                &credential(
                    5,
                    Some("aster.gateway,aster.config-admin"),
                    Some("notes.read"),
                ),
                json!({}),
            )),
            CLIENT,
            None,
        )
        .unwrap();
        enroll(rt, edge, server_node);
        let conn = connect(rt, edge_node, server_node);

        // No entry: the edge calls as itself.
        let me = call(rt, conn, "whoami", None).unwrap();
        assert_eq!(me["principal"], id(5));
        assert_eq!(me["on_behalf_of"], false);
        assert_eq!(me["actor"], "");
        assert_eq!(me["forwarded"], json!([]));
        assert_eq!(
            call(rt, conn, "read", None).unwrap_err(),
            PERMISSION_DENIED,
            "the edge itself lacks notes.read"
        );

        // Minted for alice with a passkey: the handler sees the user.
        let token = mint(
            rt,
            edge,
            json!({"sub": "alice", "roles": ["notes.read"], "amr": ["webauthn"], "ttl_secs": 60}),
        )
        .unwrap();
        let alice = call(rt, conn, "read", Some(&token)).unwrap();
        assert_eq!(alice["principal"], format!("{}|alice", id(5)));
        assert_eq!(alice["on_behalf_of"], true);
        assert_eq!(alice["actor"], id(5));
        let forwarded = alice["forwarded"].as_array().unwrap();
        assert_eq!(entry(forwarded, "iss"), [id(5).as_str()]);
        assert_eq!(entry(forwarded, "sub"), ["alice"]);
        assert_eq!(entry(forwarded, "amr"), ["webauthn"]);
        assert_eq!(entry(forwarded, "jti").len(), 1);
        assert!(entry(forwarded, "exp")[0].parse::<u64>().unwrap() > now());
        // ABI 7.9: the verified token's whole claim set, as one JSON object.
        let claims: Value = serde_json::from_str(entry(forwarded, "claims")[0]).unwrap();
        assert_eq!(claims["sub"], "alice");
        assert_eq!(claims["iss"], id(5));
        assert_eq!(claims["amr"], json!(["webauthn"]));
        assert_eq!(
            entry(alice["attributes"].as_array().unwrap(), "aster.role"),
            ["notes.read"]
        );

        // Minting outside the delegation, or too long, is refused locally.
        let refused = mint(
            rt,
            edge,
            json!({"sub": "bob", "roles": ["notes.admin"], "amr": ["pwd"], "ttl_secs": 60}),
        )
        .unwrap_err();
        assert!(refused.contains("issuer delegation"), "{refused}");
        assert!(mint(
            rt,
            edge,
            json!({"sub": "bob", "amr": ["pwd"], "ttl_secs": 3600})
        )
        .is_err());
        // The server has no publisher; nor does the edge.
        assert_eq!(
            aster_enrollment_publisher_notice(rt, edge, bytes(b"https://idp"), bytes(b"k1")),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        // NodeConfig get: the effective identity settings and their sources.
        let view = node_config(rt, conn, GET, json!({}));
        assert_eq!(view["code"], 0, "{view}");
        let view = &view["result"];
        assert_eq!(view["node_id"], id(2));
        assert_eq!(view["generation"], 0);
        let accept = view["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == "identity.accept_on_behalf_of")
            .unwrap();
        assert_eq!(accept["value"], true);
        assert_eq!(accept["source"], "code");

        // Set: properties for handlers, and on-behalf-of turned off.
        let ack = node_config(
            rt,
            conn,
            SET,
            json!({
                "name": "notes-1",
                "tags": ["eu"],
                "attributes": {"tier": {"type": "int", "value": 3}},
                "sections": {"identity": {"accept_on_behalf_of": false}},
            }),
        );
        assert_eq!(ack["code"], 0, "{ack}");
        assert_eq!(ack["result"]["complete"], true);
        assert_eq!(ack["result"]["generation"], 1);
        assert_eq!(aster_server_node_config_generation(srv), 1);
        let me = call(rt, conn, "whoami", None).unwrap();
        assert_eq!(me["properties"]["name"], "notes-1");
        assert_eq!(me["properties"]["generation"], 1);
        assert_eq!(
            me["properties"]["attributes"]["tier"],
            json!({"type": "int", "value": 3})
        );
        assert_eq!(
            call(rt, conn, "read", Some(&token)).unwrap_err(),
            PERMISSION_DENIED,
            "accept_on_behalf_of = false refuses the entry"
        );
        let ack = node_config(rt, conn, ACK, json!({"generation": 1}));
        assert_eq!(ack["result"]["complete"], true, "{ack}");
        let view = node_config(rt, conn, GET, json!({}));
        assert_eq!(view["result"]["changed_by"], id(5));
        assert_eq!(
            view["result"]["sections"]["identity"],
            json!({"accept_on_behalf_of": false})
        );

        // A set the server refuses keeps its code; a malformed one fails
        // before it is sent.
        let refused = node_config(
            rt,
            conn,
            SET,
            json!({"sections": {"network": {"bind_port": 1}}}),
        );
        assert_eq!(refused["code"], PERMISSION_DENIED, "{refused}");
        let text = json!({"sections": {"identity": {"bogus": 1}}}).to_string();
        let mut op = 0;
        assert_eq!(
            aster_node_config_call(rt, conn, SET, bytes(text.as_bytes()), 0, &mut op),
            INVALID
        );
        assert_eq!(
            aster_node_config_call(rt, conn, 9, bytes(b"{}"), 0, &mut op),
            INVALID
        );

        // Clear: back to the file settings; the token works again.
        let ack = node_config(rt, conn, CLEAR, json!({}));
        assert_eq!(ack["result"]["complete"], true, "{ack}");
        let alice = call(rt, conn, "read", Some(&token)).unwrap();
        assert_eq!(alice["actor"], id(5));
        assert_eq!(alice["properties"]["name"], "");

        stop.store(true, Ordering::SeqCst);
        poller.join().unwrap();
        assert_eq!(aster_server_destroy(rt, srv), OK);
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
    root.tokio.block_on(root.node.clone().shutdown());
}

/// The server builder validates `[identity]` and `[remote_config]` as
/// `AsterServer` does; NodeConfig off refuses the audit hook.
#[test]
fn server_identity_settings_are_validated_on_the_node_config() {
    unsafe {
        let rt = runtime();
        let issuers = br#"[{"iss":"https://idp.example","audiences":["svc"]}]"#;
        let lock: &'static [aster_bytes_t] = Box::leak(Box::new([bytes(b"network")]));
        type Edit = Box<dyn Fn(&mut aster_identity_config_t)>;
        let cases: Vec<(&str, Edit)> = vec![
            (
                "needs [enrollment]",
                Box::new(|i| i.trusted_issuers_json = bytes(issuers)),
            ),
            (
                "not an overridable section",
                Box::new(move |i| {
                    i.remote_config_lock = aster_bytes_list_t {
                        items: lock.as_ptr(),
                        len: 1,
                    };
                }),
            ),
            (
                "edge_token_max_lifetime_secs",
                Box::new(|i| {
                    i.set = ASTER_IDENTITY_SET_EDGE_TOKEN_MAX_LIFETIME;
                    i.edge_token_max_lifetime_secs = 0;
                }),
            ),
        ];
        for (want, apply) in cases {
            let mut identity = identity_view();
            apply(&mut identity);
            let (node, _) = start(rt, 3, None, 0, Some(&identity)).unwrap();
            let mut builder = 0;
            assert_eq!(aster_server_builder_new(rt, node, &mut builder), INVALID);
            assert!(last_error().contains(want), "{want}: {}", last_error());
            assert_eq!(aster_node_free(rt, node), OK);
        }

        // [identity.publisher] without Enrollment is refused at node start.
        let mut identity = identity_view();
        let publisher = br#"{"issuers":["https://idp.example"],"jwks_url":{"https://idp.example":"https://idp.example/jwks"}}"#;
        identity.publisher_json = bytes(publisher);
        let (status, message) = start(rt, 3, None, 0, Some(&identity)).unwrap_err();
        assert_eq!(status, INVALID);
        assert!(message.contains("[identity.publisher]"), "{message}");

        // NodeConfig off: not served, and the audit hook is refused.
        let mut identity = identity_view();
        identity.remote_config_disabled = 1;
        let (node, _) = start(rt, 3, None, 0, Some(&identity)).unwrap();
        let mut builder = 0;
        assert_eq!(aster_server_builder_new(rt, node, &mut builder), OK);
        assert_eq!(
            aster_server_set_audit_outbox(rt, builder, 424242),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        let mut srv = 0;
        assert_eq!(aster_server_start(rt, builder, 16, &mut srv), OK);
        let mut props = bytes(b"");
        assert_eq!(aster_server_node_properties(srv, &mut props), OK);
        let json: Value = serde_json::from_slice(&copy(props.ptr, props.len as u32)).unwrap();
        aster_string_release(props.ptr, props.len);
        assert_eq!(
            json,
            json!({"generation": 0, "name": "", "tags": [], "attributes": {}})
        );
        let (client, _) = start(rt, 4, None, 0, None).unwrap();
        let conn = connect(rt, client, node);
        let view = node_config(rt, conn, GET, json!({}));
        assert_ne!(view["code"], 0, "NodeConfig is not served: {view}");

        // A plain (unenrolled) server serves NodeConfig, but nobody can hold
        // aster.config-admin: every method is refused.
        let (plain, _) = start(rt, 6, None, 0, None).unwrap();
        let mut builder = 0;
        assert_eq!(aster_server_builder_new(rt, plain, &mut builder), OK);
        let mut plain_srv = 0;
        assert_eq!(aster_server_start(rt, builder, 16, &mut plain_srv), OK);
        let conn = connect(rt, client, plain);
        let view = node_config(rt, conn, GET, json!({}));
        assert_eq!(view["code"], PERMISSION_DENIED, "{view}");

        assert_eq!(aster_server_destroy(rt, srv), OK);
        assert_eq!(aster_server_destroy(rt, plain_srv), OK);
        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
}

/// The edge's issuer-key publisher status, as JSON per issuer.
unsafe fn publisher_status(rt: aster_runtime_t, edge: u64) -> Value {
    let mut out = bytes(b"");
    assert_eq!(aster_enrollment_publisher_status(rt, edge, &mut out), OK);
    let json = serde_json::from_slice(&copy(out.ptr, out.len as u32)).unwrap();
    aster_string_release(out.ptr, out.len);
    json
}

/// A client's `[identity.publisher]` starts on its first enrollment and keeps
/// running when the client enrolls with another server (added as a delivery
/// target): its status, here two failed fetches, carries on instead of
/// starting over (FFI-7).
#[test]
fn a_client_publisher_is_not_restarted_per_enrolled_server() {
    const ISS: &str = "https://idp.example";
    let root = root();
    let files = tempfile::tempdir().unwrap();
    unsafe {
        let rt = runtime();
        let server = |seed: u8, name: &str| {
            let (node, _) = start(
                rt,
                seed,
                Some(&trust(
                    &root,
                    &credential(seed, None, None),
                    json!({"state_path": files.path().join(name).to_str().unwrap()}),
                )),
                SERVER,
                None,
            )
            .unwrap();
            // The server answers the enrollment handshake.
            let mut builder = 0;
            assert_eq!(aster_server_builder_new(rt, node, &mut builder), OK);
            let mut srv = 0;
            assert_eq!(aster_server_start(rt, builder, 16, &mut srv), OK);
            node
        };
        let (a, b) = (server(2, "a.json"), server(3, "b.json"));
        // Nothing listens at the JWKS URL: every attempt fails fast, then backs
        // off (BACKOFF_BASE: at least 2.5 s before the second attempt).
        let mut identity = identity_view();
        let publisher =
            json!({"issuers": [ISS], "jwks_url": {ISS: "https://127.0.0.1:1/jwks"}}).to_string();
        identity.publisher_json = bytes(publisher.as_bytes());
        let (_edge_node, edge) = start(
            rt,
            6,
            Some(&trust(&root, &credential(6, None, None), json!({}))),
            CLIENT,
            Some(&identity),
        )
        .unwrap();
        // No publisher before the first enrollment.
        let mut none = bytes(b"");
        assert_eq!(
            aster_enrollment_publisher_status(rt, edge, &mut none),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        enroll(rt, edge, a);
        let attempts = |s: &Value| s[ISS]["attempts"].as_u64().unwrap_or(0);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = publisher_status(rt, edge);
            if attempts(&status) >= 2 {
                assert!(status[ISS]["last_error"].is_string(), "{status}");
                break;
            }
            assert!(Instant::now() < deadline, "no second attempt: {status}");
            std::thread::sleep(Duration::from_millis(100));
        }

        // A restarted publisher would start its status over (at most one
        // attempt before its first back-off).
        enroll(rt, edge, b);
        let status = publisher_status(rt, edge);
        assert!(attempts(&status) >= 2, "publisher restarted: {status}");

        assert_eq!(
            aster_runtime_close_and_wait(rt, Duration::from_secs(10)),
            OK
        );
    }
    root.tokio.block_on(root.node.clone().shutdown());
}
