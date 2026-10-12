//! Opt-in Linux qualification: real server, enrollment, exact-root Docs, audit,
//! privileged helper and systemd supervisor. Requires passwordless sudo. Uses
//! only generated temporary identities; never reads the user's Aster profile.
#[cfg(target_os = "linux")]
mod linux {
    use aster::audit_outbox::wire::{BatchRequest, DurableAck, batch_id, verify_wire_event};
    use aster::enrollment::*;
    use aster::{AsterConfig, Node, NodeAddr, RelayMode, SecretKey};
    use aster_exec::{
        contract::*,
        policy::{ExecGrantV1, ExecOperationV1, GrantStatusV1, grant_key},
    };
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
    fn sudo(args: &[&str]) -> Result<()> {
        if !Command::new("sudo")
            .arg("-n")
            .args(args)
            .status()?
            .success()
        {
            return Err(format!("sudo command failed: {}", args[0]).into());
        }
        Ok(())
    }
    fn config(secret: SecretKey) -> AsterConfig {
        AsterConfig::builder()
            .relay(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .secret_key(secret)
            .hooks(true)
            .hook_failure_mode(aster::HookFailureMode::FailClosed)
            .build()
    }
    fn bytes(id: &str) -> [u8; 32] {
        std::array::from_fn(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).unwrap())
    }
    fn token(root: &SecretKey, subject: [u8; 32]) -> String {
        let mut request = EnrollmentIssueRequest::new(subject, "exec-full-smoke");
        request.expires_at = Some(now() + 3600);
        issue_jwt(root, &request, now()).unwrap().into_jwt()
    }
    fn stop_unit_quietly(unit: &str) {
        for operation in ["stop", "reset-failed"] {
            let _ = Command::new("sudo")
                .args(["-n", "systemctl", operation, unit])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    struct Cleanup {
        child: Option<Child>,
        helper_unit: String,
        root: PathBuf,
        jobs: Vec<String>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = Command::new("kill")
                    .args(["-TERM", &child.id().to_string()])
                    .status();
                std::thread::sleep(Duration::from_secs(1));
                let _ = child.kill();
                let _ = child.wait();
            }
            for job in &self.jobs {
                stop_unit_quietly(job);
            }
            stop_unit_quietly(&self.helper_unit);
            if self.root.is_absolute()
                && self
                    .root
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("aster-exec-e2e-")
            {
                let _ = sudo(&[
                    "rm",
                    "-rf",
                    self.root.to_str().unwrap(),
                    self.root.with_extension("work").to_str().unwrap(),
                ]);
            }
        }
    }
    struct Receiver {
        endpoint: String,
        events: Arc<Mutex<Vec<serde_json::Value>>>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Receiver {
        fn start(path: &Path) -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            listener.set_nonblocking(true)?;
            let endpoint = format!("http://{}/audit", listener.local_addr()?);
            let events = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (e, s, p) = (events.clone(), stop.clone(), path.to_owned());
            let worker = std::thread::spawn(move || {
                let mut retained = std::collections::BTreeMap::<String, Vec<u8>>::new();
                while !s.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let result = (|| -> Result<()> {
                                stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                                let mut reader = BufReader::new(stream.try_clone()?);
                                let mut length = 0;
                                for _ in 0..32 {
                                    let mut line = String::new();
                                    reader.by_ref().take(4097).read_line(&mut line)?;
                                    if line == "\r\n" {
                                        break;
                                    }
                                    if let Some((key, value)) = line.split_once(':')
                                        && key.eq_ignore_ascii_case("content-length")
                                    {
                                        length = value.trim().parse()?;
                                    }
                                }
                                if length == 0 || length > 1_420_000 {
                                    return Err("receiver length".into());
                                }
                                let mut body = vec![0; length];
                                reader.read_exact(&mut body)?;
                                let batch: BatchRequest = serde_json::from_slice(&body)?;
                                if batch.version != 1
                                    || batch.batch_id
                                        != batch_id(
                                            batch.events.iter().map(|e| e.event_id.as_str()),
                                        )
                                {
                                    return Err("receiver batch".into());
                                }
                                let mut file =
                                    fs::OpenOptions::new().create(true).append(true).open(&p)?;
                                for event in &batch.events {
                                    let (envelope, original) = verify_wire_event(event, 65536)?;
                                    if let Some(previous) = retained.get(&event.event_id) {
                                        if previous != &original {
                                            return Err(
                                                "receiver duplicate integrity conflict".into()
                                            );
                                        }
                                        continue;
                                    }
                                    file.write_all(&original)?;
                                    file.write_all(b"\n")?;
                                    retained.insert(event.event_id.clone(), original);
                                    e.lock().unwrap().push(serde_json::to_value(envelope)?);
                                }
                                file.sync_all()?;
                                let ack = serde_json::to_vec(&DurableAck {
                                    version: 1,
                                    batch_id: batch.batch_id,
                                    durable: true,
                                })?;
                                write!(
                                    stream,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    ack.len()
                                )?;
                                stream.write_all(&ack)?;
                                Ok(())
                            })();
                            if let Err(error) = result {
                                eprintln!("receiver: {error}");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(Self {
                endpoint,
                events,
                stop,
                worker: Some(worker),
            })
        }
    }
    impl Drop for Receiver {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
    async fn terminal(client: &aster_exec_client::Client, job: &JobRefV1) -> Result<JobStatusV1> {
        for _ in 0..150 {
            match client.status(job).await {
                Ok(status) if status.state.is_terminal() => return Ok(status),
                Ok(_) => {}
                Err(aster_exec_client::Error::Exec(error)) if error.retryable => {}
                Err(error) => return Err(error.into()),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("terminal status timeout".into())
    }
    pub async fn run() -> Result<()> {
        let started = std::time::Instant::now();
        let mut args = std::env::args_os().skip(1);
        let binaries = PathBuf::from(args.next().ok_or("expected target/debug path")?);
        let work = PathBuf::from(args.next().ok_or("expected temporary fixture directory")?);
        if args.next().is_some() {
            return Err("unexpected argument".into());
        }
        fs::create_dir_all(&work)?;
        fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let root_base = std::env::var_os("ASTER_EXEC_QUALIFICATION_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib"));
        if !root_base.is_absolute() {
            return Err("qualification root must be absolute".into());
        }
        let root_path = root_base.join(format!("aster-exec-e2e-{}", &nonce[..12]));
        // Helper socket parents become 0710 root:APIgroup. Keep payload cwd in
        // a separate 0755 sibling so traversal uses the payload's permissions.
        let payload_work = root_path.with_extension("work");
        let helper_unit = format!("aster-exec-e2e-helper-{nonce}.service");
        let mut cleanup = Cleanup {
            child: None,
            helper_unit: helper_unit.clone(),
            root: root_path.clone(),
            jobs: vec![],
        };
        sudo(&["mkdir", "-m", "0755", root_path.to_str().unwrap()])?;
        for name in ["aster-exec-helper", "aster-exec-supervisor"] {
            sudo(&[
                "install",
                "-m",
                "0755",
                binaries.join(name).to_str().unwrap(),
                root_path.join(name).to_str().unwrap(),
            ])?;
        }
        sudo(&[
            "mkdir",
            "-m",
            "0755",
            payload_work.clone().to_str().unwrap(),
        ])?;
        sudo(&[
            "mkdir",
            "-m",
            "0700",
            root_path.join("jobs").to_str().unwrap(),
        ])?;
        let uid = Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8(uid.stdout)?.trim().parse::<u32>()?;
        let gid = Command::new("id").arg("-g").output()?;
        let gid = String::from_utf8(gid.stdout)?.trim().parse::<u32>()?;
        let limits = serde_json::json!({"cpu_millis_per_second":1000,"memory_bytes":33554432,"processes":16,"file_bytes":1048576});
        let socket = root_path.join("helper.sock");
        let helper_config = serde_json::json!({"socket":socket,"state_root":root_path.join("jobs"),"supervisor":root_path.join("aster-exec-supervisor"),"systemd_run":"/usr/bin/systemd-run","api_uid":uid,"api_gid":gid,"payload_uid":65534,"payload_gid":65534,"payload_groups":[],"resource_limits":limits});
        fs::write(
            work.join("helper.json"),
            serde_json::to_vec(&helper_config)?,
        )?;
        sudo(&[
            "install",
            "-m",
            "0600",
            work.join("helper.json").to_str().unwrap(),
            root_path.join("helper.json").to_str().unwrap(),
        ])?;
        sudo(&[
            "systemd-run",
            "--quiet",
            "--collect",
            "--unit",
            &helper_unit,
            "--property=RuntimeMaxSec=180",
            "--",
            root_path.join("aster-exec-helper").to_str().unwrap(),
            root_path.join("helper.json").to_str().unwrap(),
        ])?;
        let root_key = SecretKey::generate();
        let root_id = hex_id(JwtEnrollmentIssuer::from_seed(&root_key.to_bytes()).public_key());
        let mut root_config = config(root_key.clone());
        root_config.enable_hooks = true;
        let root_node = Node::start_with_alpns(root_config, vec![ENROLLMENT_ALPN.to_vec()]).await?;
        for _ in 0..100 {
            if !root_node.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let document = root_node.docs().create().await?;
        let namespace = document.id()?.to_string();
        let api_key = SecretKey::generate();
        let api_id = hex_id(JwtEnrollmentIssuer::from_seed(&api_key.to_bytes()).public_key());
        let caller_key = SecretKey::generate();
        let caller_id = hex_id(JwtEnrollmentIssuer::from_seed(&caller_key.to_bytes()).public_key());
        let operations = vec![
            ExecOperationV1::Discover,
            ExecOperationV1::Submit,
            ExecOperationV1::Status,
            ExecOperationV1::Output,
            ExecOperationV1::Watch,
            ExecOperationV1::Cancel,
        ];
        let grant = ExecGrantV1 {
            version: 1,
            issuer: root_id.clone(),
            audience: "exec-full-smoke".into(),
            caller: caller_id.clone(),
            target: api_id.clone(),
            revision: 1,
            issued_at: now(),
            not_before: None,
            expires_at: Some(now() + 3600),
            status: GrantStatusV1::Active,
            account_alias: "nobody".into(),
            profiles: vec!["open".into()],
            operations: operations.clone(),
            cwd_roots: vec![payload_work.clone().to_string_lossy().into()],
            max_timeout_ms: 60000,
            max_output_bytes: 65536,
            cancel_on_revocation: true,
            reauthorizes_revision: None,
        };
        let author = root_node.docs().default_author().await?;
        assert_eq!(
            author.to_string(),
            root_id,
            "grant writer must be exact root"
        );
        document
            .set_bytes(
                &author,
                grant_key(&bytes(&caller_id), &bytes(&api_id)),
                serde_json::to_vec(&grant)?,
            )
            .await?;
        let mut local_store = aster_exec::policy::ExecGrantStore::open(
            &work.join("root-grant-check.redb"),
            bytes(&root_id),
            bytes(&namespace),
            "exec-full-smoke".into(),
        )?;
        local_store
            .refresh_one(document.as_core(), &bytes(&caller_id), &bytes(&api_id))
            .await?;
        assert_eq!(
            local_store
                .grant_for(&bytes(&caller_id), &bytes(&api_id), now())?
                .revision,
            1
        );
        let ticket = document.share_with_addr(aster::ShareMode::Read).await?;
        let root_enrollment = Enrollment::install(
            &root_node,
            Arc::new(EnrollmentPolicyStore::open(
                work.join("replica-policy.json"),
                bytes(&root_id),
                bytes(&namespace),
                "exec-full-smoke",
            )?),
            JwtEnrollmentVerifier::new(bytes(&root_id), "exec-full-smoke")?,
            Some(token(&root_key, bytes(&root_id))),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )?;
        root_enrollment.set_policy_document(&document)?;
        let root_handle = aster::rpc::Server::new(&root_node)
            .route_alpn(ENROLLMENT_ALPN, move |connection| {
                let enrollment = root_enrollment.clone();
                tokio::spawn(async move {
                    if enrollment.accept(&connection).await.is_ok() {
                        connection.closed().await;
                    }
                });
            })
            .serve();
        let state = work.join("api");
        fs::create_dir_all(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        api_key.write_file(state.join("node.key"))?;
        for (name, value) in [
            ("api.jwt", token(&root_key, bytes(&api_id))),
            ("policy.ticket", ticket),
        ] {
            fs::write(work.join(name), value)?;
            sudo(&[
                "install",
                "-m",
                "0640",
                "-g",
                &gid.to_string(),
                work.join(name).to_str().unwrap(),
                root_path.join(name).to_str().unwrap(),
            ])?;
        }
        let receiver = Receiver::start(&work.join("audit-received.jsonl"))?;
        let port = std::net::UdpSocket::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let server_config = serde_json::json!({"state_directory":state,"bind_address":format!("127.0.0.1:{port}"),"root_public_key":root_id,"policy_namespace":namespace,"policy_replicas":[{"node_id":root_node.id().to_string(),"relay_url":root_node.addr().relay_url,"direct_addresses":root_node.addr().direct_addresses}],"audience":"exec-full-smoke","policy_read_ticket_file":root_path.join("policy.ticket"),"credential_file":root_path.join("api.jwt"),"helper_socket":socket,"ceiling":{"version":1,"target_id":api_id,"account_alias":"nobody","default_profile":"open","enabled_profiles":["open"],"enabled_operations":operations,"cwd_roots":[payload_work.clone()],"max_timeout_ms":60000,"max_output_bytes":65536,"denied_callers":[]},"resource_limits":limits,"audit":{"directory":state.join("audit"),"destination":{"endpoint":receiver.endpoint},"max_retained_bytes":1048576,"max_event_bytes":65536},"reconcile_interval_ms":1000});
        fs::write(
            work.join("server.json"),
            serde_json::to_vec(&server_config)?,
        )?;
        sudo(&[
            "install",
            "-m",
            "0640",
            "-g",
            &gid.to_string(),
            work.join("server.json").to_str().unwrap(),
            root_path.join("server.json").to_str().unwrap(),
        ])?;
        cleanup.child = Some(
            Command::new(binaries.join("aster-exec-server"))
                .arg(root_path.join("server.json"))
                .stdout(Stdio::null())
                .stderr(fs::File::create(work.join("server.log"))?)
                .spawn()?,
        );
        let caller =
            Node::start_with_alpns(config(caller_key), vec![ENROLLMENT_ALPN.to_vec()]).await?;
        let enrollment = Enrollment::install(
            &caller,
            Arc::new(EnrollmentPolicyStore::open(
                work.join("caller-policy.json"),
                bytes(&root_id),
                bytes(&namespace),
                "exec-full-smoke",
            )?),
            JwtEnrollmentVerifier::new(bytes(&root_id), "exec-full-smoke")?,
            Some(token(&root_key, bytes(&caller_id))),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )?;
        let address = NodeAddr {
            node_id: api_id.parse()?,
            relay_url: None,
            direct_addresses: vec![format!("127.0.0.1:{port}")],
        };
        let mut admitted = false;
        for _ in 0..30 {
            if enrollment.connect(address.clone()).await.is_ok() {
                admitted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !admitted {
            return Err("real enrollment admission failed; inspect server.log".into());
        }
        caller.add_node_addr(address)?;
        let client = aster_exec_client::Client::new(caller.rpc_connect(&api_id.parse()?).await?);
        let mut ready = false;
        for attempt in 0..90 {
            let discovery = client.targets().await;
            if attempt % 20 == 0 {
                eprintln!("grant readiness attempt {attempt}: {discovery:?}");
            }
            if discovery.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !ready {
            return Err("exact-root grant unavailable".into());
        }
        let caps = client
            .capabilities(&TargetRefV1 {
                target_id: api_id.clone(),
            })
            .await?;
        if !caps.backend_ready {
            return Err("real helper not ready".into());
        }
        let mut request = aster_exec_client::request(
            &api_id,
            payload_work.clone().to_string_lossy(),
            "/usr/bin/printf",
            vec!["full-stack-smoke\\n".into()],
        );
        request.execution_timeout_ms = Some(60000);
        request.output_limit_bytes = Some(65536);
        let job = client.submit(&request).await?;
        eprintln!("printf accepted: {}", job.job_id);
        cleanup
            .jobs
            .push(aster_exec::systemd_unit::unit_name(&job.job_id)?);
        let status = terminal(&client, &job).await?;
        let first_job_ms = started.elapsed().as_millis();
        for _ in 0..20 {
            assert_eq!(client.status(&job).await?.state, JobStateV1::Succeeded);
        }
        if status.state != JobStateV1::Succeeded || status.exit_code != Some(0) {
            return Err(format!("printf status: {status:?}").into());
        }
        let output = client
            .output(&OutputRequestV1 {
                job: job.clone(),
                cursor: 0,
                max_bytes: 4096,
            })
            .await?;
        let output: Vec<u8> = output
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        if output != b"full-stack-smoke\n" {
            return Err(format!("output mismatch: {output:?}").into());
        }
        assert_eq!(client.submit(&request).await?, job);
        let mut identity_request = aster_exec_client::request(
            &api_id,
            payload_work.clone().to_string_lossy(),
            "/usr/bin/id",
            vec!["-u".into()],
        );
        identity_request.execution_timeout_ms = Some(60000);
        identity_request.output_limit_bytes = Some(65536);
        let identity_job = client.submit(&identity_request).await?;
        cleanup
            .jobs
            .push(aster_exec::systemd_unit::unit_name(&identity_job.job_id)?);
        assert_eq!(
            terminal(&client, &identity_job).await?.state,
            JobStateV1::Succeeded
        );
        let uid_output = client
            .output(&OutputRequestV1 {
                job: identity_job.clone(),
                cursor: 0,
                max_bytes: 4096,
            })
            .await?;
        let uid_bytes: Vec<u8> = uid_output
            .chunks
            .into_iter()
            .flat_map(|chunk| chunk.bytes)
            .collect();
        assert_eq!(
            uid_bytes, b"65534\n",
            "payload must execute as distinct unprivileged UID"
        );
        let mut request = aster_exec_client::request(
            &api_id,
            payload_work.clone().to_string_lossy(),
            "/usr/bin/sleep",
            vec!["30".into()],
        );
        request.execution_timeout_ms = Some(60000);
        request.output_limit_bytes = Some(65536);
        let sleeping = client.submit(&request).await?;
        cleanup
            .jobs
            .push(aster_exec::systemd_unit::unit_name(&sleeping.job_id)?);
        tokio::time::sleep(Duration::from_millis(500)).await;
        client
            .cancel(&CancelRequestV1 {
                job: sleeping.clone(),
                grace_ms: Some(100),
            })
            .await?;
        eprintln!("cancel receipt received");
        let cancelled = terminal(&client, &sleeping).await?;
        if cancelled.state != JobStateV1::Cancelled {
            return Err(format!("cancel status: {cancelled:?}").into());
        }
        eprintln!("cancel terminal confirmed");
        // Allocate beyond the payload's 32 MiB cgroup limit. The supervisor
        // lives in a separate leaf and must survive to persist the outcome.
        let mut request = aster_exec_client::request(
            &api_id,
            payload_work.to_string_lossy(),
            "/usr/bin/python3",
            vec![
                "-c".into(),
                "allocation = bytearray(64 * 1024 * 1024)".into(),
            ],
        );
        request.execution_timeout_ms = Some(60000);
        request.output_limit_bytes = Some(65536);
        let memory_job = client.submit(&request).await?;
        cleanup
            .jobs
            .push(aster_exec::systemd_unit::unit_name(&memory_job.job_id)?);
        let memory_status = terminal(&client, &memory_job).await?;
        assert_eq!(memory_status.state, JobStateV1::Failed);
        assert_eq!(memory_status.reason_code.as_deref(), Some("memory_limit"));
        assert!(memory_status.finished_at_ms.is_some());
        eprintln!("payload OOM terminal persisted: failed/memory_limit");
        for _ in 0..200 {
            let delivered = {
                let events = receiver.events.lock().unwrap();
                events
                    .iter()
                    .filter(|event| event["kind"] == "exec.intent.v1")
                    .count()
                    >= 4
                    && events
                        .iter()
                        .filter(|event| event["kind"] == "exec.outcome.v1")
                        .count()
                        >= 4
                    && events
                        .iter()
                        .any(|event| event["kind"] == "exec.cancel.intent.v1")
            };
            if delivered {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let events = receiver.events.lock().unwrap().clone();
        let kinds: Vec<_> = events
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_owned())
            .collect();
        if kinds.iter().filter(|k| *k == "exec.intent.v1").count() != 4
            || kinds.iter().filter(|k| *k == "exec.outcome.v1").count() < 4
            || !kinds.iter().any(|k| k == "exec.cancel.intent.v1")
        {
            return Err(format!("audit kinds incomplete: {kinds:?}").into());
        }
        assert!(
            events.iter().any(|event| event["kind"] == "exec.outcome.v1"
                && event["payload"]["job_id"] == memory_job.job_id
                && event["payload"]["state"] == "failed"
                && event["payload"]["reason_code"] == "memory_limit"
                && event["payload"]["finished_at_ms"].is_u64()),
            "OOM terminal outcome must be durably received and acknowledged"
        );
        for event in &events {
            assert!(event["payload"].is_object());
            match event["kind"].as_str().unwrap() {
                "exec.intent.v1" => {
                    assert_eq!(event["payload"]["caller"], caller_id);
                    assert_eq!(event["payload"]["target"], api_id);
                }
                "exec.outcome.v1" => {
                    assert_eq!(event["payload"]["target_id"], api_id);
                    assert!(event["payload"]["finished_at_ms"].is_u64());
                }
                "exec.cancel.intent.v1" => {
                    assert_eq!(event["payload"]["job_id"], sleeping.job_id);
                }
                other => panic!("unexpected audit event {other}"),
            }
        }
        println!(
            "PASS real enrollment + exact-root Docs + Exec RPC + privileged systemd runtime: submit/output/status/idempotent retry/cancel/payload OOM; durable receiver audit kinds={kinds:?}; setup_to_first_job_ms={first_job_ms}; warm_status_calls=20; jobs={},{}",
            job.job_id, sleeping.job_id
        );
        caller.shutdown().await;
        root_handle.abort();
        root_node.shutdown().await;
        drop(cleanup);
        Ok(())
    }
    fn hex_id(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(std::time::Duration::from_secs(150), linux::run()).await?
}
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("Linux only")
}
