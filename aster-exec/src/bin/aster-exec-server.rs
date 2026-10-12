//! Unprivileged Linux Exec RPC host. Privileged execution stays in the helper.
#[cfg(target_os = "linux")]
mod linux {
    use aster::audit_outbox::{AuditOutbox, OutboxConfig};
    use aster::enrollment::*;
    use aster::rpc::{RPC_ALPN, Server};
    use aster::{AsterConfig, HookFailureMode, Node};
    use aster_exec::{
        authorization::LocalCeilingV1,
        contract::*,
        ledger::RequestLedger,
        linux_adapter::LinuxRuntime,
        policy::ExecGrantStore,
        service::{DocsGrantProvider, ExecService},
    };
    use serde::Deserialize;
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
        path::PathBuf,
        sync::Arc,
        time::Duration,
    };
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        state_directory: PathBuf,
        bind_address: String,
        root_public_key: String,
        policy_namespace: String,
        policy_replicas: Vec<Replica>,
        audience: String,
        policy_read_ticket_file: PathBuf,
        credential_file: PathBuf,
        helper_socket: PathBuf,
        ceiling: LocalCeilingV1,
        resource_limits: ResourceLimitsV1,
        audit: OutboxConfig,
        reconcile_interval_ms: u64,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Replica {
        node_id: String,
        relay_url: Option<String>,
        direct_addresses: Vec<String>,
    }
    fn identity(value: &str) -> Result<[u8; 32]> {
        Ok(aster_exec::policy::parse_node_id(value)
            .ok_or("identity must be 64 lowercase hex characters")?)
    }
    fn private_file(path: &std::path::Path, limit: u64) -> Result<Vec<u8>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o027 != 0 {
            return Err(
                "configuration and credentials must be root-owned and mode 0640 or stricter".into(),
            );
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err("configuration exceeds size limit".into());
        }
        Ok(bytes)
    }
    pub async fn run() -> Result<()> {
        if unsafe { libc::geteuid() } == 0 {
            return Err("Exec API must run as an unprivileged account".into());
        }
        let mut args = std::env::args_os().skip(1);
        let path = PathBuf::from(args.next().ok_or("expected configuration file")?);
        if args.next().is_some() {
            return Err("unexpected argument".into());
        }
        let config: Config = serde_json::from_slice(&private_file(&path, 65536)?)?;
        let root = identity(&config.root_public_key)?;
        let namespace = identity(&config.policy_namespace)?;
        if !config.state_directory.is_absolute()
            || !config.helper_socket.is_absolute()
            || !(200..=2000).contains(&config.reconcile_interval_ms)
            || config.ceiling.version != 1
            || config.ceiling.enabled_profiles != ["open".to_string()].into()
            || config.ceiling.default_profile != "open"
            || config.ceiling.account_alias.is_empty()
            || config
                .ceiling
                .denied_callers
                .iter()
                .any(|caller| identity(caller).is_err())
            || config.policy_replicas.is_empty()
            || config.policy_replicas.len() > 16
            || !config.audit.directory.starts_with(&config.state_directory)
            || config.ceiling.cwd_roots.is_empty()
            || config
                .ceiling
                .cwd_roots
                .iter()
                .any(|root| !root.is_absolute() || !root.is_dir())
            || config.ceiling.max_timeout_ms == 0
            || config.ceiling.max_timeout_ms > MAX_TIMEOUT_MS
            || config.ceiling.max_output_bytes == 0
            || config.ceiling.max_output_bytes > MAX_OUTPUT_BYTES
        {
            return Err("invalid local Exec configuration".into());
        }
        let credential = String::from_utf8(private_file(&config.credential_file, 65536)?)?;
        let ticket = String::from_utf8(private_file(&config.policy_read_ticket_file, 65536)?)?;
        std::fs::create_dir_all(&config.state_directory)?;
        let node_config = AsterConfig {
            storage_path: Some(config.state_directory.join("node")),
            secret_key_file: Some(config.state_directory.join("node.key")),
            bind_addr: Some(config.bind_address),
            enable_hooks: true,
            hook_failure_mode: HookFailureMode::FailClosed,
            gc_interval: Some(Duration::from_secs(300)),
            ..AsterConfig::default()
        };
        let node = Node::start_with_alpns(
            node_config,
            vec![RPC_ALPN.to_vec(), ENROLLMENT_ALPN.to_vec()],
        )
        .await?;
        if node.id().to_string() != config.ceiling.target_id {
            return Err("local ceiling target differs from persistent node identity".into());
        }
        let enrollment = Enrollment::install(
            &node,
            Arc::new(EnrollmentPolicyStore::open(
                config.state_directory.join("enrollment.json"),
                root,
                namespace,
                &config.audience,
            )?),
            JwtEnrollmentVerifier::new(root, &config.audience)?,
            Some(credential.trim().into()),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )?;
        let replicas = config
            .policy_replicas
            .into_iter()
            .map(|replica| {
                Ok(aster::NodeAddr {
                    node_id: replica.node_id.parse()?,
                    relay_url: replica.relay_url,
                    direct_addresses: replica.direct_addresses,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (readiness, offered_ticket) = enrollment.connect_replicas(replicas).await?;
        let document = enrollment
            .synchronize(
                readiness.peer,
                offered_ticket.unwrap_or_else(|| ticket.trim().to_string()),
                Duration::from_secs(15),
            )
            .await?;
        if document.id()?.to_string() != config.policy_namespace {
            return Err("policy ticket namespace differs from pinned namespace".into());
        }
        let audit = Arc::new(AuditOutbox::open(config.audit).await?);
        let guarantees = GuaranteesV1 {
            profile: "open".into(),
            provider: "linux-systemd-cgroup-v2".into(),
            network: NetworkGuaranteeV1::Inherited,
            proxy_id: None,
            filesystem: FilesystemGuaranteeV1::Inherited,
            filesystem_policy_id: None,
            execution_identity: config.ceiling.account_alias.clone(),
            resource_limits: config.resource_limits,
            descendant_scope: DescendantScopeV1::Cgroup,
            restart_survival: RestartSurvivalV1::ApiRestart,
        };
        let caps = TargetCapabilitiesV1 {
            target_id: config.ceiling.target_id.clone(),
            node_id: node.id().to_string(),
            os: TargetOsV1::Linux,
            architecture: std::env::consts::ARCH.into(),
            environment: TargetEnvironmentV1::Native,
            path_flavor: PathFlavorV1::Posix,
            account_alias: config.ceiling.account_alias.clone(),
            os_principal: config.ceiling.account_alias.clone(),
            profiles: vec![AvailableProfileV1 {
                id: "open".into(),
                guarantees: guarantees.clone(),
            }],
            adapters: vec![],
            max_execution_timeout_ms: config.ceiling.max_timeout_ms,
            max_output_bytes: config.ceiling.max_output_bytes,
            max_stdin_bytes: MAX_STDIN_BYTES as u64,
            request_retention_ms: REQUEST_RETENTION_MS,
            output_retention_ms: OUTPUT_RETENTION_MS,
            backend_ready: false,
            unavailable_reason: None,
        };
        let service = ExecService::new(
            config.ceiling,
            Arc::new(enrollment.clone()),
            Arc::new(DocsGrantProvider {
                store: Arc::new(ExecGrantStore::open(
                    &config.state_directory.join("grants.redb"),
                    root,
                    namespace,
                    config.audience,
                )?),
                document,
            }),
            audit.clone(),
            Arc::new(LinuxRuntime {
                socket: config.helper_socket,
                target_capabilities: caps,
                guarantees,
            }),
            RequestLedger::open(&config.state_directory.join("requests.redb"))?,
        );
        // Reconcile before accepting RPCs. Callers have no enrollment session
        // yet, which leaves their running jobs untouched; helper failure is
        // reported but does not remove the endpoint needed for diagnostics.
        if let Err(failure) = service.reconcile_policy().await {
            eprintln!("Exec startup reconciliation: {:?}", failure.code);
        }
        let routed = enrollment.clone();
        let server = Server::new(&node)
            .handler_timeout(None)
            .register(service.clone())
            .route_alpn(ENROLLMENT_ALPN, move |connection| {
                let enrollment = routed.clone();
                tokio::spawn(async move {
                    if enrollment.accept(&connection).await.is_ok() {
                        connection.closed().await;
                    }
                });
            })
            .serve();
        eprintln!("Aster Exec node {}", node.id());
        let refresh_interval = Duration::from_millis(config.reconcile_interval_ms);
        let revocations = enrollment.revocations();
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let maintenance_failed = tokio::select! {
            result = tokio::signal::ctrl_c() => { result?; false },
            _ = terminate.recv() => false,
            failure = service.maintain(refresh_interval, revocations) => {
                eprintln!("Exec maintenance stopped: {}", failure.safe_message);
                true
            },
        };
        server.abort();
        let _ = audit.flush(Duration::from_secs(5)).await;
        audit.close().await?;
        node.shutdown().await;
        if maintenance_failed {
            return Err("Exec maintenance task stopped".into());
        }
        Ok(())
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn deployment_json_examples_match_configuration_schemas() {
            let guide = include_str!("../../../docs/aster-exec-linux.md");
            let examples: Vec<&str> = guide
                .split("```json\n")
                .skip(1)
                .map(|part| part.split("```").next().unwrap())
                .collect();
            assert_eq!(examples.len(), 2);
            serde_json::from_str::<aster_exec::linux_helper::HelperConfig>(examples[0]).unwrap();
            serde_json::from_str::<Config>(examples[1]).unwrap();
        }
    }
}
#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run().await
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("aster-exec-server is Linux-only");
    std::process::exit(2);
}
