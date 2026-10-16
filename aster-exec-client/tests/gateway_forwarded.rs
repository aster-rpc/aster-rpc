//! Gateway identity, slice 7: an edge holding `aster.gateway` submits to
//! Exec on behalf of a user with an edge-minted token. The core verifies the
//! token; Exec runs the job under the grant naming the user (`iss|sub`) and
//! audits the user as principal and the edge as actor. The fleet runs on the
//! high-level path (`AsterServer`, `AsterClient`, `AsterConfig`); Exec's
//! grants, admission and runtime are in-memory fixtures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aster::audit_outbox::AuditEvent;
use aster::enrollment::*;
use aster::rpc::identity::ISSUABLE_ROLES_ATTRIBUTE;
use aster::rpc::{AsterClient, AsterServer, GATEWAY_ROLE, StatusCode};
use aster::{AsterConfig, Doc, EnrollmentConfig, Node, PublicKey, RelayMode, SecretKey};
use aster_exec::{
    authorization::{AuthorizedSubmit, LocalCeilingV1},
    contract::*,
    ledger::RequestLedger,
    policy::{ExecGrantV1, ExecOperationV1, GrantStatusV1, grant_subject},
    service::*,
};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

const FLEET: &str = "exec-gateway-fleet";
const USER_ROLE: &str = "exec.user";

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

/// A root-issued credential with `roles` and, when given, an issuer
/// delegation of `issuable` roles.
fn credential(seed: u8, roles: Option<&str>, issuable: Option<&str>) -> String {
    let mut attributes = BTreeMap::new();
    if let Some(roles) = roles {
        attributes.insert("aster.role".into(), TypedAttribute::String(roles.into()));
    }
    if let Some(issuable) = issuable {
        attributes.insert(
            ISSUABLE_ROLES_ATTRIBUTE.into(),
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

fn base(seed: u8) -> aster::AsterConfigBuilder {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .secret_key(key(seed))
        .root_pubkey(PublicKey::from_bytes(public(1)))
}

fn namespace(doc: &Doc) -> String {
    doc.id().unwrap().to_string()
}

async fn wait_for_addr(node: &Node) {
    for _ in 0..100 {
        if !node.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connect(fleet: &Doc, server: &AsterServer, seed: u8, chain: String) -> AsterClient {
    let config = base(seed)
        .enrollment_credential_chain(chain)
        .enrollment(EnrollmentConfig {
            audience: Some(FLEET.into()),
            fleet_namespace: Some(namespace(fleet)),
            ..Default::default()
        })
        .build();
    AsterClient::builder()
        .config(config)
        .address(server.address().unwrap())
        .connect()
        .await
        .unwrap()
}

fn jti_of(token: &str) -> String {
    let payload = token.split('.').nth(1).unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    json["jti"].as_str().unwrap().to_string()
}

/// Exec admission is not under test: every node is enrolled.
struct Admitted;
#[async_trait]
impl EnrollmentAdmission for Admitted {
    async fn check(&self, _: &[u8; 32]) -> Result<(), AdmissionError> {
        Ok(())
    }
    fn delegate_revoked(&self, _: &[u8; 32]) -> bool {
        false
    }
}

/// Grants by subject; a missing one is a definite denial.
struct Grants(BTreeMap<[u8; 32], ExecGrantV1>);
#[async_trait]
impl GrantProvider for Grants {
    async fn current(
        &self,
        subject: [u8; 32],
        _: [u8; 32],
        _: u64,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        self.0
            .get(&subject)
            .cloned()
            .ok_or_else(|| error(ExecErrorCodeV1::PermissionDenied, "no grant"))
    }
}

#[derive(Default)]
struct Audit(Mutex<Vec<AuditEvent>>);
#[async_trait]
impl ExecAudit for Audit {
    async fn admit(&self, event: AuditEvent) -> Result<(), ExecErrorV1> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

#[derive(Default)]
struct Runtime(Mutex<BTreeMap<String, JobStatusV1>>);
#[async_trait]
impl ExecRuntime for Runtime {
    async fn capabilities(&self) -> Result<TargetCapabilitiesV1, ExecErrorV1> {
        Err(error(ExecErrorCodeV1::UnsupportedOperation, "fixture"))
    }
    async fn launch(
        &self,
        request: ExecRequestV1,
        authorized: AuthorizedSubmit,
        job_id: String,
        _: [u8; 32],
        caller: String,
        accepted_at_ms: u64,
    ) -> Result<JobStatusV1, ExecErrorV1> {
        let status = JobStatusV1 {
            job: JobRefV1 {
                job_id: job_id.clone(),
                target_id: request.target_id.clone(),
            },
            state: JobStateV1::Succeeded,
            caller_id: caller,
            principal_id: None,
            resolved_target: request.target_id,
            execution_identity: authorized.account_alias.clone(),
            accepted_at_ms,
            started_at_ms: Some(accepted_at_ms),
            finished_at_ms: Some(accepted_at_ms),
            exit_code: Some(0),
            signal: None,
            reason_code: Some("exited".into()),
            output_cursor: 0,
            guarantees: GuaranteesV1 {
                profile: authorized.profile,
                provider: "test".into(),
                network: NetworkGuaranteeV1::Inherited,
                proxy_id: None,
                filesystem: FilesystemGuaranteeV1::Inherited,
                filesystem_policy_id: None,
                execution_identity: authorized.account_alias,
                resource_limits: ResourceLimitsV1 {
                    cpu_millis_per_second: None,
                    memory_bytes: None,
                    processes: None,
                    file_bytes: None,
                },
                descendant_scope: DescendantScopeV1::Cgroup,
                restart_survival: RestartSurvivalV1::ApiRestart,
            },
        };
        self.0.lock().unwrap().insert(job_id, status.clone());
        Ok(status)
    }
    async fn status(&self, job: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
        self.0
            .lock()
            .unwrap()
            .get(&job.job_id)
            .cloned()
            .ok_or_else(|| error(ExecErrorCodeV1::JobNotFound, "no such job"))
    }
    async fn output(&self, _: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
        Err(error(ExecErrorCodeV1::UnsupportedOperation, "fixture"))
    }
    async fn cancel(&self, _: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
        Err(error(ExecErrorCodeV1::UnsupportedOperation, "fixture"))
    }
    async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
        Ok(self.0.lock().unwrap().values().cloned().collect())
    }
    async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
        Ok(vec![])
    }
    async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
        Ok(())
    }
}

fn grant(caller: &str, target: &str, cwd: &str) -> ExecGrantV1 {
    ExecGrantV1 {
        version: 1,
        issuer: id(1),
        audience: FLEET.into(),
        caller: caller.into(),
        target: target.into(),
        revision: 1,
        issued_at: 1,
        not_before: None,
        expires_at: None,
        status: GrantStatusV1::Active,
        account_alias: "builder".into(),
        profiles: vec!["open".into()],
        operations: vec![
            ExecOperationV1::Discover,
            ExecOperationV1::Submit,
            ExecOperationV1::Status,
        ],
        cwd_roots: vec![cwd.into()],
        max_timeout_ms: DEFAULT_TIMEOUT_MS,
        max_output_bytes: DEFAULT_OUTPUT_BYTES,
        cancel_on_revocation: true,
        reauthorizes_revision: None,
        require_request_digest: false,
    }
}

fn exec_code<T: std::fmt::Debug>(result: aster_exec_client::Result<T>) -> ExecErrorCodeV1 {
    match result {
        Err(aster_exec_client::Error::Exec(failure)) => failure.code,
        other => panic!("expected an Exec refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_gateway_submits_for_a_user_under_the_grant_naming_them() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let files = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(files.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let root = Node::start(base(1).build()).await.unwrap();
        let fleet = root.docs().create().await.unwrap();

        // The target is node 2; the edge, node 5, mints `<edge>|alice`.
        let (target, edge_id) = (id(2), id(5));
        let alice = format!("{edge_id}|alice");
        let grants =
            BTreeMap::from([(grant_subject(&alice).unwrap(), grant(&alice, &target, &cwd))]);
        let audit = Arc::new(Audit::default());
        let service = ExecService::new(
            LocalCeilingV1 {
                version: 1,
                target_id: target.clone(),
                account_alias: "builder".into(),
                default_profile: "open".into(),
                enabled_profiles: ["open".into()].into(),
                enabled_operations: [
                    ExecOperationV1::Discover,
                    ExecOperationV1::Submit,
                    ExecOperationV1::Status,
                ]
                .into(),
                cwd_roots: vec![cwd.clone().into()],
                max_timeout_ms: DEFAULT_TIMEOUT_MS,
                max_output_bytes: DEFAULT_OUTPUT_BYTES,
                denied_callers: Default::default(),
            },
            Arc::new(Admitted),
            Arc::new(Grants(grants)),
            audit.clone(),
            Arc::new(Runtime::default()),
            RequestLedger::open(&files.path().join("requests.redb")).unwrap(),
        );
        let config = base(2)
            .enrollment_credential_chain(credential(2, None, None))
            .enrollment(EnrollmentConfig {
                audience: Some(FLEET.into()),
                fleet_namespace: Some(namespace(&fleet)),
                state_path: Some(files.path().join("server.json")),
                ..Default::default()
            })
            .accept_on_behalf_of(true)
            .build();
        let server = AsterServer::builder()
            .service(service)
            .config(config)
            .start()
            .await
            .unwrap();
        wait_for_addr(server.node()).await;

        let edge = connect(
            &fleet,
            &server,
            5,
            credential(5, Some(GATEWAY_ROLE), Some(USER_ROLE)),
        )
        .await;
        assert_eq!(edge.id().to_string(), edge_id);
        let issuer = edge.user_token_issuer().unwrap();
        let token = issuer
            .mint_user_token("alice", [USER_ROLE], ["webauthn"], Duration::from_secs(120))
            .unwrap();
        let as_edge = aster_exec_client::Client::new(edge.rpc());
        let as_alice = as_edge.clone().on_behalf_of(&token);

        // The edge holds no grant of its own.
        let request = aster_exec_client::request(&target, &cwd, "/bin/true", vec![]);
        assert_eq!(
            exec_code(as_edge.submit(&request).await),
            ExecErrorCodeV1::PermissionDenied
        );

        // Alice, through the edge, runs under her grant.
        let job = as_alice.submit(&request).await.unwrap();
        let status = as_alice.status(&job).await.unwrap();
        assert_eq!(status.caller_id, alice);
        assert_eq!(status.principal_id, None);
        assert_eq!(
            exec_code(as_edge.status(&job).await),
            ExecErrorCodeV1::PermissionDenied,
            "the edge cannot read alice's job as itself"
        );
        let intent = audit
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|event| event.kind == "exec.intent.v1")
            .unwrap()
            .payload
            .clone();
        assert_eq!(intent["principal"], alice);
        assert_eq!(intent["actor"], edge_id);
        assert_eq!(intent["forwarded"]["issuer"], edge_id);
        assert_eq!(intent["forwarded"]["jti"], jti_of(&token));
        assert_eq!(intent["forwarded"]["auth_methods"][0], "webauthn");

        // Bob holds no grant.
        let bob = issuer
            .mint_user_token("bob", [USER_ROLE], ["pwd"], Duration::from_secs(120))
            .unwrap();
        let request = aster_exec_client::request(&target, &cwd, "/bin/true", vec![]);
        assert_eq!(
            exec_code(as_edge.clone().on_behalf_of(&bob).submit(&request).await),
            ExecErrorCodeV1::PermissionDenied
        );

        // A peer without `aster.gateway` cannot forward alice's token: the
        // core refuses it before Exec runs.
        let other = connect(&fleet, &server, 6, credential(6, Some(USER_ROLE), None)).await;
        let replayed = aster_exec_client::Client::new(other.rpc()).on_behalf_of(&token);
        match replayed.submit(&request).await {
            Err(aster_exec_client::Error::Transport(failure)) => {
                assert_eq!(failure.rpc_code(), Some(StatusCode::PermissionDenied));
            }
            other => panic!("expected a transport refusal, got {other:?}"),
        }

        other.close().await;
        edge.close().await;
        server.shutdown().await;
        root.shutdown().await;
    })
    .await
    .unwrap();
}
