//! Real authenticated Iroh/RPC transport; deterministic policy/runtime fixtures.
use aster::audit_outbox::AuditEvent;
use aster::rpc::{RPC_ALPN, Server, StreamHeader};
use aster::{AsterConfig, Node, RelayMode};
use aster_exec::{
    authorization::{AuthorizedSubmit, LocalCeilingV1},
    contract::*,
    ledger::RequestLedger,
    policy::{ExecGrantV1, ExecOperationV1, GrantStatusV1},
    service::*,
};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;
struct Admission(Vec<[u8; 32]>);
#[async_trait]
impl EnrollmentAdmission for Admission {
    async fn check(&self, caller: &[u8; 32]) -> Result<(), ExecErrorV1> {
        if self.0.contains(caller) {
            Ok(())
        } else {
            Err(error(ExecErrorCodeV1::PermissionDenied, "not enrolled"))
        }
    }
}
struct Grants(Mutex<ExecGrantV1>);
#[async_trait]
impl GrantProvider for Grants {
    async fn current(
        &self,
        caller: [u8; 32],
        target: [u8; 32],
        _: u64,
    ) -> Result<ExecGrantV1, ExecErrorV1> {
        let mut grant = self.0.lock().unwrap().clone();
        assert_eq!(hex_id(target), grant.target);
        grant.caller = hex_id(caller);
        Ok(grant)
    }
}
fn hex_id(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
struct Runtime(Mutex<Option<JobStatusV1>>);
fn guarantees() -> GuaranteesV1 {
    GuaranteesV1 {
        profile: "open".into(),
        provider: "test".into(),
        network: NetworkGuaranteeV1::Inherited,
        proxy_id: None,
        filesystem: FilesystemGuaranteeV1::Inherited,
        filesystem_policy_id: None,
        execution_identity: "builder".into(),
        resource_limits: ResourceLimitsV1 {
            cpu_millis_per_second: None,
            memory_bytes: None,
            processes: None,
            file_bytes: None,
        },
        descendant_scope: DescendantScopeV1::Cgroup,
        restart_survival: RestartSurvivalV1::ApiRestart,
    }
}
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
        assert_eq!(request.argv, ["a b", "$(whoami)", ""]);
        assert_eq!(authorized.account_alias, "builder");
        let status = JobStatusV1 {
            job: JobRefV1 {
                job_id,
                target_id: request.target_id.clone(),
            },
            state: JobStateV1::Succeeded,
            caller_id: caller,
            resolved_target: request.target_id,
            execution_identity: "builder".into(),
            accepted_at_ms,
            started_at_ms: Some(accepted_at_ms),
            finished_at_ms: Some(accepted_at_ms),
            exit_code: Some(0),
            signal: None,
            reason_code: Some("exited".into()),
            output_cursor: 0,
            guarantees: guarantees(),
        };
        *self.0.lock().unwrap() = Some(status.clone());
        Ok(status)
    }
    async fn status(&self, job: &JobRefV1) -> Result<JobStatusV1, ExecErrorV1> {
        let s = self.0.lock().unwrap().clone().unwrap();
        assert_eq!(job, &s.job);
        Ok(s)
    }
    async fn output(&self, _: &OutputRequestV1) -> Result<OutputPageV1, ExecErrorV1> {
        panic!("denied output must not reach runtime")
    }
    async fn cancel(&self, _: &CancelRequestV1) -> Result<CancelReceiptV1, ExecErrorV1> {
        panic!("denied cancellation must not reach runtime")
    }
    async fn list_jobs(&self) -> Result<Vec<JobStatusV1>, ExecErrorV1> {
        Ok(self.0.lock().unwrap().clone().into_iter().collect())
    }
    async fn pending_outcomes(&self) -> Result<Vec<(String, Vec<u8>)>, ExecErrorV1> {
        Ok(vec![])
    }
    async fn ack_outcome(&self, _: &str) -> Result<(), ExecErrorV1> {
        Ok(())
    }
}
fn config() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}
#[tokio::test]
async fn authenticated_rpc_retry_ownership_malformed_payload_and_grant_denial() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let root = tempfile::tempdir().unwrap();
        let server = Node::start_with_alpns(config(), vec![RPC_ALPN.to_vec()])
            .await
            .unwrap();
        let caller = Node::start(config()).await.unwrap();
        let stranger = Node::start(config()).await.unwrap();
        let other = Node::start(config()).await.unwrap();
        for _ in 0..100 {
            if !server.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        caller.add_peer(&server).unwrap();
        stranger.add_peer(&server).unwrap();
        other.add_peer(&server).unwrap();
        let target = server.id().to_string();
        let caller_id = caller.id().to_string();
        let caller_bytes =
            std::array::from_fn(|i| u8::from_str_radix(&caller_id[i * 2..i * 2 + 2], 16).unwrap());
        let operations = vec![
            ExecOperationV1::Discover,
            ExecOperationV1::Submit,
            ExecOperationV1::Status,
            ExecOperationV1::Output,
            ExecOperationV1::Cancel,
        ];
        let grant = ExecGrantV1 {
            version: 1,
            issuer: "11".repeat(32),
            audience: "test".into(),
            caller: caller_id.clone(),
            target: target.clone(),
            revision: 1,
            issued_at: 1,
            not_before: None,
            expires_at: None,
            status: GrantStatusV1::Active,
            account_alias: "builder".into(),
            profiles: vec!["open".into()],
            operations: operations.clone(),
            cwd_roots: vec![root.path().to_string_lossy().into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            cancel_on_revocation: true,
            reauthorizes_revision: None,
        };
        let ceiling = LocalCeilingV1 {
            version: 1,
            target_id: target.clone(),
            account_alias: "builder".into(),
            default_profile: "open".into(),
            enabled_profiles: ["open".into()].into(),
            enabled_operations: operations.into_iter().collect(),
            cwd_roots: vec![root.path().into()],
            max_timeout_ms: DEFAULT_TIMEOUT_MS,
            max_output_bytes: DEFAULT_OUTPUT_BYTES,
            denied_callers: Default::default(),
        };
        let audit = Arc::new(Audit::default());
        let runtime = Arc::new(Runtime::default());
        let grants = Arc::new(Grants(Mutex::new(grant)));
        let other_id = other.id().to_string();
        let other_bytes =
            std::array::from_fn(|i| u8::from_str_radix(&other_id[i * 2..i * 2 + 2], 16).unwrap());
        let service = ExecService::new(
            ceiling,
            Arc::new(Admission(vec![caller_bytes, other_bytes])),
            grants.clone(),
            audit.clone(),
            runtime,
            RequestLedger::open(&root.path().join("requests.redb")).unwrap(),
        );
        let handle = Server::new(&server).register(service.clone()).serve();
        let client =
            aster_exec_client::Client::new(caller.rpc_connect(&server.id()).await.unwrap());
        assert_eq!(
            client.targets().await.unwrap(),
            vec![TargetRefV1 {
                target_id: target.clone()
            }]
        );
        let request = aster_exec_client::request(
            &target,
            root.path().to_string_lossy(),
            "/bin/printf",
            vec!["a b".into(), "$(whoami)".into(), "".into()],
        );
        let job = client.submit(&request).await.unwrap();
        assert_eq!(client.submit(&request).await.unwrap(), job);
        let status = client.status(&job).await.unwrap();
        assert_eq!(status.state, JobStateV1::Succeeded);
        assert_eq!(status.caller_id, caller_id);
        assert_eq!(
            audit.0.lock().unwrap().len(),
            1,
            "retry must not emit another intent"
        );
        let unknown =
            aster_exec_client::Client::new(stranger.rpc_connect(&server.id()).await.unwrap());
        assert!(matches!(
            unknown.targets().await,
            Err(aster_exec_client::Error::Exec(ExecErrorV1 {
                code: ExecErrorCodeV1::PermissionDenied,
                ..
            }))
        ));
        // This peer is enrolled and independently authorized for every operation.
        // Its denial must therefore come from job ownership, not enrollment/grants.
        let other_client =
            aster_exec_client::Client::new(other.rpc_connect(&server.id()).await.unwrap());
        assert!(other_client.targets().await.is_ok());
        assert_exec_code(
            other_client.status(&job).await,
            ExecErrorCodeV1::PermissionDenied,
        );
        let output = OutputRequestV1 {
            job: job.clone(),
            cursor: 0,
            max_bytes: MIN_PAGE_BYTES,
        };
        let cancel = CancelRequestV1 {
            job: job.clone(),
            grace_ms: None,
        };
        assert_exec_code(
            other_client.output(&output).await,
            ExecErrorCodeV1::PermissionDenied,
        );
        assert_exec_code(
            other_client.cancel(&cancel).await,
            ExecErrorCodeV1::PermissionDenied,
        );

        // Bypass the typed client's validation to exercise the server's decoder.
        let raw = caller.rpc_connect(&server.id()).await.unwrap();
        let header = StreamHeader {
            service: SERVICE_NAME.into(),
            method: "submit".into(),
            version: SERVICE_VERSION as i32,
            ..Default::default()
        };
        for payload in [b"{".to_vec(), b"{}".to_vec()] {
            let reply = raw.unary(&header, payload).await.unwrap();
            let result: Result<JobRefV1, ExecErrorV1> = serde_json::from_slice(&reply).unwrap();
            assert_eq!(result.unwrap_err().code, ExecErrorCodeV1::InvalidArgument);
        }
        // Refresh the cached authority explicitly, avoiding timing-dependent sleeps.
        for (status, expires_at) in [
            (GrantStatusV1::Active, Some(1)),
            (GrantStatusV1::Revoked, None),
        ] {
            {
                let mut grant = grants.0.lock().unwrap();
                grant.status = status;
                grant.expires_at = expires_at;
            }
            service.refresh_peer(&caller_id).await.unwrap();
            assert_exec_code(
                client.submit(&request).await,
                ExecErrorCodeV1::PermissionDenied,
            );
            assert_exec_code(client.status(&job).await, ExecErrorCodeV1::PermissionDenied);
            assert_exec_code(
                client.output(&output).await,
                ExecErrorCodeV1::PermissionDenied,
            );
            assert_exec_code(
                client.cancel(&cancel).await,
                ExecErrorCodeV1::PermissionDenied,
            );
        }
        assert_eq!(
            audit.0.lock().unwrap().len(),
            1,
            "denied requests must not emit intent"
        );
        assert_eq!(other_client.targets().await.unwrap().len(), 1);
        handle.abort();
        caller.shutdown().await;
        stranger.shutdown().await;
        other.shutdown().await;
        server.shutdown().await;
    })
    .await
    .expect("RPC smoke timed out");
}

fn assert_exec_code<T: std::fmt::Debug>(
    result: aster_exec_client::Result<T>,
    code: ExecErrorCodeV1,
) {
    match result.unwrap_err() {
        aster_exec_client::Error::Exec(error) => assert_eq!(error.code, code),
        other => panic!("expected typed Exec error, got {other:?}"),
    }
}
