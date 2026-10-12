//! Private per-profile Unix socket broker. No TCP fallback or privilege changes.
use super::{DaemonCommand, Invocation, client_error};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use serde_json::{Value, json};
#[cfg(unix)]
use {
    fs2::FileExt,
    std::{
        collections::HashMap,
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        sync::Arc,
        time::Duration,
    },
    tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
        sync::{Mutex, OnceCell},
    },
};

#[cfg(unix)]
fn directory(ctx: &Ctx) -> Result<std::path::PathBuf> {
    let profile = aster_config::load(&ctx.home)?.selected_profile_name(ctx.profile.as_deref());
    aster_config::validate_profile_name(&profile)?;
    let path = ctx.home.join("exec-runtime").join(profile);
    std::fs::create_dir_all(ctx.home.join("exec-runtime"))?;
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(Error::new(
            ErrorKind::Denied,
            "Exec runtime directory must be owned by this user with mode 0700",
        ));
    }
    Ok(path)
}
#[cfg(unix)]
async fn write(stream: &mut UnixStream, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(Error::usage("local Exec message exceeds 16 MiB"));
    }
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}
#[cfg(unix)]
async fn read(stream: &mut UnixStream) -> Result<Value> {
    let len = stream.read_u32().await? as usize;
    if len > 16 * 1024 * 1024 {
        return Err(Error::usage("local Exec message exceeds 16 MiB"));
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
#[cfg(unix)]
async fn exchange(path: &std::path::Path, request: Value) -> Result<Value> {
    exchange_budget(path, request, Duration::from_secs(45)).await
}
#[cfg(unix)]
async fn exchange_budget(
    path: &std::path::Path,
    request: Value,
    budget: Duration,
) -> Result<Value> {
    let mut stream = UnixStream::connect(path).await?;
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        return Err(Error::new(
            ErrorKind::Denied,
            "local daemon is owned by another user",
        ));
    }
    write(
        &mut stream,
        &json!({"protocol":1,"request":request,"budget_ms":budget.as_millis().min(45000) as u64}),
    )
    .await?;
    let reply = tokio::time::timeout(budget, read(&mut stream))
        .await
        .map_err(|_| {
            Error::new(
                ErrorKind::Unavailable,
                "local Exec protocol deadline exceeded",
            )
        })??;
    decode_reply(reply)
}

#[cfg(unix)]
fn decode_reply(reply: Value) -> Result<Value> {
    if reply["protocol"] != 1 {
        return Err(Error::new(
            ErrorKind::Unavailable,
            "incompatible Exec daemon; explicitly stop it with its matching CLI before upgrading",
        ));
    }
    if let Some(error) = reply.get("error") {
        let kind = error
            .get("kind")
            .and_then(|value| serde_json::from_value::<ErrorKind>(value.clone()).ok())
            .unwrap_or(ErrorKind::Failure);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or("daemon failed");
        let mapped = Error::new(kind, message);
        return Err(if let Some(details) = error.get("details") {
            mapped.with_details(details.clone())
        } else {
            mapped
        });
    }
    Ok(reply["result"].clone())
}
#[cfg(unix)]
async fn ensure(ctx: &Ctx) -> Result<std::path::PathBuf> {
    let dir = directory(ctx)?;
    let socket = dir.join("client.sock");
    if exchange(&socket, json!({"operation":"ping"})).await.is_ok() {
        return Ok(socket);
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("startup.lock"))?;
    // Nonblocking OS lock with bounded startup; never identify owners by PID.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if lock.try_lock_exclusive().is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::Unavailable,
                "Exec daemon startup lock timed out",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if exchange(&socket, json!({"operation":"ping"})).await.is_ok() {
        return Ok(socket);
    }
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg("--home").arg(&ctx.home);
    if let Some(profile) = &ctx.profile {
        command.arg("--profile").arg(profile);
    }
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    command
        .args(["exec", "daemon", "foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null()).spawn()?;
    loop {
        if exchange(&socket, json!({"operation":"ping"})).await.is_ok() {
            return Ok(socket);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::Unavailable,
                "Exec daemon did not become ready; run `aster exec daemon foreground` with the same --home and --profile for diagnostics",
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
#[cfg(unix)]
pub async fn call(ctx: &Ctx, call: Invocation) -> Result<Value> {
    let submit = call.method == "submit";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    tokio::time::timeout_at(deadline, async {
        let socket = ensure(ctx).await?;
        exchange_budget(
            &socket,
            serde_json::to_value(call)?,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        .map_err(|error| {
            if submit {
                error.with_hint(
                    "If acceptance is uncertain, reconcile submission with the same request ID",
                )
            } else {
                error
            }
        })
    })
    .await
    .map_err(|_| {
        Error::new(
            ErrorKind::Unavailable,
            if submit {
                "Exec submission deadline exceeded; reconcile submission with the same request ID"
            } else {
                "Exec control deadline exceeded"
            },
        )
    })?
}
#[cfg(unix)]
pub async fn lifecycle(ctx: &Ctx, command: DaemonCommand) -> Result<()> {
    let dir = directory(ctx)?;
    let socket = dir.join("client.sock");
    match command {
        DaemonCommand::Foreground => serve(ctx, &dir).await,
        DaemonCommand::Status => {
            println!("{}", exchange(&socket, json!({"operation":"ping"})).await?);
            Ok(())
        }
        DaemonCommand::Stop => {
            exchange(&socket, json!({"operation":"stop"})).await?;
            Ok(())
        }
        DaemonCommand::Restart => {
            if socket.exists() {
                exchange(&socket, json!({"operation":"stop"})).await?;
            }
            for _ in 0..100 {
                if !socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ensure(ctx).await?;
            Ok(())
        }
    }
}
#[cfg(unix)]
pub(super) fn grants_key(path: &std::path::Path) -> Result<aster::SecretKey> {
    let bytes = zeroize::Zeroizing::new(super::grants::bounded(path, 128, true)?);
    let text =
        std::str::from_utf8(&bytes).map_err(|_| Error::usage("node key must contain hex"))?;
    let seed = super::grants::identity(text.trim())?;
    Ok(aster::SecretKey::from_bytes(seed))
}
#[cfg(unix)]
async fn serve(ctx: &Ctx, dir: &std::path::Path) -> Result<()> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("owner.lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        Error::new(
            ErrorKind::Unavailable,
            "Exec daemon already owns this context",
        )
    })?;
    let socket = dir.join("client.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let cfg = aster_config::load(&ctx.home)?;
    let profile = cfg.selected_profile_name(ctx.profile.as_deref());
    let key_path = ctx.home.join("keys").join(format!("{profile}.node.key"));
    // Loading the existing seed explicitly prevents Node::start from silently
    // generating a new caller when a provisioned key disappears.
    let key = grants_key(&key_path)?;
    let settings = cfg
        .profiles
        .get(&profile)
        .ok_or_else(|| Error::usage("selected Exec profile does not exist"))?;
    let root = super::grants::identity(
        settings
            .root_pubkey
            .as_deref()
            .ok_or_else(|| Error::usage("Exec profile requires root_pubkey"))?,
    )?;
    let namespace = super::grants::identity(
        settings
            .exec_policy_namespace
            .as_deref()
            .ok_or_else(|| Error::usage("Exec profile requires exec_policy_namespace"))?,
    )?;
    let audience = settings
        .exec_audience
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::usage("Exec profile requires exec_audience"))?;
    let credential_path = settings.exec_credential_file.as_ref().ok_or_else(|| {
        Error::usage("Exec profile requires exec_credential_file (aster enroll node --out output)")
    })?;
    if !credential_path.is_absolute() {
        return Err(Error::usage("exec_credential_file must be absolute"));
    }
    let credential = zeroize::Zeroizing::new(String::from_utf8(super::grants::bounded(
        credential_path,
        65536,
        true,
    )?)?);
    let config = aster::AsterConfig {
        secret_key: Some(key),
        root_pubkey: Some(aster::PublicKey::from_hex(&hex::encode(root))?),
        enable_hooks: true,
        hook_failure_mode: aster::HookFailureMode::FailClosed,
        ..Default::default()
    };
    let node = Arc::new(
        aster::Node::start_with_alpns(config, vec![aster::enrollment::ENROLLMENT_ALPN.to_vec()])
            .await?,
    );
    let direct_addresses = Arc::new(settings.exec_direct_addresses.clone());
    let enrollment = Arc::new(aster::enrollment::Enrollment::install(
        &node,
        Arc::new(aster::enrollment::EnrollmentPolicyStore::open(
            dir.join("enrollment.json"),
            root,
            namespace,
            audience,
        )?),
        aster::enrollment::JwtEnrollmentVerifier::new(root, audience)?,
        Some(credential.trim().into()),
        Arc::new(aster::enrollment::PolicyExpressionV1::Predicate(
            aster::enrollment::PolicyPredicate::TolerateStalePolicy,
        )),
        aster::enrollment::EnrollmentOptions::default(),
    )?);
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let pool = Arc::new(Mutex::new(HashMap::<
        String,
        Arc<OnceCell<aster_exec_client::Client>>,
    >::new()));
    let mut tasks = tokio::task::JoinSet::new();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let mut idle_deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(idle_deadline), if tasks.is_empty() => break,
            _ = shutdown.notified() => break,
            connection = listener.accept() => {
                let (mut stream,_) = match connection {
                    Ok(connection) => connection,
                    Err(error) => { eprintln!("Exec daemon accept failed: {error}"); continue; }
                };
                idle_deadline = tokio::time::Instant::now() + Duration::from_secs(600);
                if !stream.peer_cred().is_ok_and(|cred| cred.uid() == unsafe { libc::geteuid() }) { continue; }
                let node = node.clone(); let pool = pool.clone(); let shutdown = shutdown.clone(); let enrollment = enrollment.clone(); let direct_addresses = direct_addresses.clone();
                tasks.spawn(async move {
                    let outcome: Result<Value> = async {
                        let envelope = tokio::time::timeout(Duration::from_secs(10),read(&mut stream)).await.map_err(|_|Error::usage("local request deadline exceeded"))??;
                        if envelope["protocol"] != 1 { return Err(Error::usage("incompatible local Exec protocol")); }
                        let req = &envelope["request"];
                        if req["operation"] == "ping" { return Ok(json!({"ready":true,"protocol":1,"idle_timeout_seconds":600})); }
                        if req["operation"] == "stop" { shutdown.notify_one(); return Ok(json!({"stopping":true})); }
                        let budget = Duration::from_millis(envelope["budget_ms"].as_u64().unwrap_or(40000).clamp(1, 45000));
                        tokio::time::timeout(budget, async {
                        let invocation: Invocation = serde_json::from_value(req.clone())?;
                        let cell = {
                            let mut pool = pool.lock().await;
                            pool.entry(invocation.node.clone()).or_default().clone()
                        };
                        let client = cell.get_or_try_init(|| async {
                            let peer = invocation.node.parse()?;
                            tokio::time::timeout(Duration::from_secs(15), async {
                                let address = aster::NodeAddr { node_id: peer, relay_url: None, direct_addresses: direct_addresses.get(&invocation.node).cloned().unwrap_or_default() };
                                node.add_node_addr(address.clone())?;
                                enrollment.connect(address).await?;
                                let peer = invocation.node.parse()?;
                                Ok::<_, Error>(aster_exec_client::Client::new(node.rpc_connect(&peer).await?))
                            }).await.map_err(|_| Error::new(ErrorKind::Unavailable,"remote enrollment/connection deadline exceeded"))?
                        }).await?.clone();
                        let p = invocation.request;
                        macro_rules! invoke { ($method:ident) => { match client.$method(&serde_json::from_value(p)?).await { Ok(value) => serde_json::to_value(value)?, Err(error) => { if matches!(&error,aster_exec_client::Error::Transport(_)) { pool.lock().await.remove(&invocation.node); } return Err(client_error(error)); } } }; }
                        Ok(match invocation.method.as_str() {
                            "targets" => match client.targets().await {Ok(value)=>serde_json::to_value(value)?,Err(error)=>{if matches!(&error,aster_exec_client::Error::Transport(_)){pool.lock().await.remove(&invocation.node);}return Err(client_error(error));}},
                            "watch" => {
                                let req: aster_exec_client::WatchRequestV1 = serde_json::from_value(p)?;
                                let page_request=aster_exec_client::OutputRequestV1 {job:req.job.clone(),cursor:req.cursor,max_bytes:aster_exec_client::MAX_PAGE_BYTES};
                                let page=match client.output(&page_request).await{Ok(value)=>value,Err(error)=>{if matches!(&error,aster_exec_client::Error::Transport(_)){pool.lock().await.remove(&invocation.node);}return Err(client_error(error));}};
                                let status=match client.status(&req.job).await{Ok(value)=>value,Err(error)=>{if matches!(&error,aster_exec_client::Error::Transport(_)){pool.lock().await.remove(&invocation.node);}return Err(client_error(error));}};
                                json!({"output":page,"status":status})
                            },
                            "capabilities" => invoke!(capabilities), "submit" => invoke!(submit), "status" => invoke!(status), "output" => invoke!(output), "cancel" => invoke!(cancel),
                            _ => return Err(Error::usage("unknown Exec operation")),
                        })
                        }).await.unwrap_or_else(|_| Err(Error::new(ErrorKind::Unavailable,
                            "Exec operation deadline exceeded; reconcile any submission with the same request ID")))
                    }.await;
                    let reply = match outcome { Ok(value) => json!({"protocol":1,"result":value}),Err(error)=>{let mut value=error.to_json();value["protocol"]=json!(1);value} };
                    let _ = write(&mut stream,&reply).await;
                });
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => { idle_deadline = tokio::time::Instant::now() + Duration::from_secs(600); }
        }
    }
    drop(listener);
    while tasks.join_next().await.is_some() {}
    std::fs::remove_file(socket)?;
    Ok(())
}
#[cfg(not(unix))]
pub async fn call(_: &Ctx, _: Invocation) -> Result<Value> {
    Err(Error::new(
        ErrorKind::Unavailable,
        "local Exec broker requires a supported Unix socket platform",
    ))
}
#[cfg(not(unix))]
pub async fn lifecycle(_: &Ctx, _: DaemonCommand) -> Result<()> {
    Err(Error::new(
        ErrorKind::Unavailable,
        "local Exec broker requires a supported Unix socket platform",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn missing_caller_key_is_never_generated() {
        let path = std::env::temp_dir().join(format!("missing-exec-key-{}", uuid::Uuid::new_v4()));
        assert!(grants_key(&path).is_err());
        assert!(!path.exists());
    }
    #[test]
    fn structured_local_error_roundtrip_preserves_exec_code_retry_and_denial_kind() {
        let original = super::super::client_error(aster_exec_client::Error::Exec(
            aster_exec_client::ExecErrorV1 {
                code: aster_exec_client::ExecErrorCodeV1::SupervisorUnavailable,
                safe_message: "control socket pending".into(),
                request_id: None,
                job_id: Some("job".into()),
                retryable: true,
                retry_after_ms: Some(100),
            },
        ));
        let mut reply = original.to_json();
        reply["protocol"] = json!(1);
        let wire = serde_json::to_vec(&reply).unwrap();
        let recovered = decode_reply(serde_json::from_slice(&wire).unwrap()).unwrap_err();
        assert_eq!(recovered.details(), original.details());
        assert_eq!(
            recovered.details().unwrap()["exec"]["code"],
            "supervisor_unavailable"
        );
        let denied = Error::new(ErrorKind::Denied, "denied");
        let mut reply = denied.to_json();
        reply["protocol"] = json!(1);
        assert_eq!(decode_reply(reply).unwrap_err().kind(), ErrorKind::Denied);
        assert!(
            decode_reply(json!({"protocol":1,"error":"legacy daemon failure"}))
                .unwrap_err()
                .details()
                .is_none()
        );
    }
    #[tokio::test]
    async fn daemon_reuses_owner_and_restarts_without_remote_job_cancellation() {
        let home = std::path::PathBuf::from("/tmp").join(format!("ae-{}", uuid::Uuid::new_v4()));
        let ctx = Ctx::new(&aster_cli_kit::GlobalArgs {
            home: Some(home.clone()),
            profile: None,
            json: true,
            color: aster_cli_kit::ctx::ColorChoice::Never,
        })
        .unwrap();
        let key = aster::SecretKey::generate();
        std::fs::create_dir_all(home.join("keys")).unwrap();
        key.write_file(home.join("keys/default.node.key")).unwrap();
        let root_key = aster::SecretKey::generate();
        let issuer = aster::enrollment::JwtEnrollmentIssuer::from_seed(&root_key.to_bytes());
        let caller_id =
            aster::enrollment::JwtEnrollmentIssuer::from_seed(&key.to_bytes()).public_key();
        let request = aster::enrollment::EnrollmentIssueRequest::new(caller_id, "test");
        let token = issuer.issue(&request, 1).unwrap();
        let credential = home.join("credential.jwt");
        std::fs::write(&credential, token.jwt()).unwrap();
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
        let server_config = aster::AsterConfig {
            secret_key: Some(root_key.clone()),
            bind_addr: Some("127.0.0.1:0".parse().unwrap()),
            relay_mode: aster::RelayMode::Disabled,
            enable_hooks: true,
            hook_failure_mode: aster::HookFailureMode::FailClosed,
            ..Default::default()
        };
        let server = aster::Node::start_with_alpns(
            server_config,
            vec![
                aster::rpc::RPC_ALPN.to_vec(),
                aster::enrollment::ENROLLMENT_ALPN.to_vec(),
            ],
        )
        .await
        .unwrap();
        let document = server.docs().create().await.unwrap();
        let namespace =
            super::super::grants::identity(&document.id().unwrap().to_string()).unwrap();
        let server_token = issuer
            .issue(
                &aster::enrollment::EnrollmentIssueRequest::new(issuer.public_key(), "test"),
                1,
            )
            .unwrap();
        let admission = aster::enrollment::Enrollment::install(
            &server,
            Arc::new(
                aster::enrollment::EnrollmentPolicyStore::open(
                    home.join("server-policy.json"),
                    issuer.public_key(),
                    namespace,
                    "test",
                )
                .unwrap(),
            ),
            aster::enrollment::JwtEnrollmentVerifier::new(issuer.public_key(), "test").unwrap(),
            Some(server_token.jwt().into()),
            Arc::new(aster::enrollment::PolicyExpressionV1::Predicate(
                aster::enrollment::PolicyPredicate::TolerateStalePolicy,
            )),
            aster::enrollment::EnrollmentOptions::default(),
        )
        .unwrap();
        admission.set_policy_document(&document).unwrap();
        let accepting = admission.clone();
        let server_handle = aster::rpc::Server::new(&server)
            .route_alpn(aster::enrollment::ENROLLMENT_ALPN, move |connection| {
                let enrollment = accepting.clone();
                tokio::spawn(async move {
                    if enrollment.accept(&connection).await.is_ok() {
                        connection.closed().await;
                    }
                });
            })
            .serve();
        for _ in 0..200 {
            if !server.addr().direct_addresses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        aster_config::update(&home, |cfg| -> Result<()> {
            cfg.profiles.insert(
                "default".into(),
                aster_config::Profile {
                    root_pubkey: Some(hex::encode(issuer.public_key())),
                    exec_credential_file: Some(credential),
                    exec_audience: Some("test".into()),
                    exec_policy_namespace: Some(hex::encode(namespace)),
                    exec_direct_addresses: [(
                        server.id().to_string(),
                        server.addr().direct_addresses,
                    )]
                    .into(),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        let dir = directory(&ctx).unwrap();
        let socket = dir.join("client.sock");
        for _ in 0..2 {
            let task_ctx = ctx.clone();
            let task_dir = dir.clone();
            let owner = tokio::spawn(async move { serve(&task_ctx, &task_dir).await });
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            if !socket.exists() {
                panic!("daemon failed to initialize: {:?}", owner.await.unwrap());
            }
            let (first, second) = tokio::join!(
                exchange(&socket, json!({"operation":"ping"})),
                exchange(&socket, json!({"operation":"ping"}))
            );
            assert_eq!(first.unwrap(), second.unwrap());
            let started = tokio::time::Instant::now();
            let unavailable = serde_json::to_value(
                super::super::invocation("44".repeat(32), "targets", ()).unwrap(),
            )
            .unwrap();
            let (bounded, ping) = tokio::join!(
                exchange_budget(&socket, unavailable, Duration::from_millis(20)),
                exchange(&socket, json!({"operation":"ping"})),
            );
            assert!(bounded.is_err());
            assert!(
                ping.is_ok(),
                "unreachable target must not block broker liveness"
            );
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "daemon must honor caller's remaining deadline"
            );
            // No Exec service is registered here: the RPC must reach dispatch,
            // and the real enrollment session must exist before that call.
            let response = exchange(
                &socket,
                serde_json::to_value(
                    super::super::invocation(server.id().to_string(), "targets", ()).unwrap(),
                )
                .unwrap(),
            )
            .await;
            assert!(response.is_err());
            admission
                .check_peer(&caller_id)
                .await
                .expect("daemon must complete real enrollment before RPC");
            assert!(
                serve(&ctx, &dir).await.is_err(),
                "second owner must not replace an active daemon"
            );
            assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o600);
            exchange(&socket, json!({"operation":"stop"}))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), owner)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!socket.exists());
        }
        server_handle.abort();
        server_handle.joined().await;
        server.shutdown().await;
        std::fs::remove_dir_all(home).unwrap();
    }
    #[tokio::test]
    async fn local_protocol_is_framed_and_bounded() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write(
            &mut a,
            &json!({"protocol":1,"request":{"argv":["a b","$(id)"]}}),
        )
        .await
        .unwrap();
        assert_eq!(
            read(&mut b).await.unwrap()["request"]["argv"],
            json!(["a b", "$(id)"])
        );
        a.write_u32(16 * 1024 * 1024 + 1).await.unwrap();
        assert!(read(&mut b).await.is_err());
    }
}
