//! Explicit root-author publication; never substitute newest-writer Docs queries.
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use aster_exec::policy::{ExecGrantV1, GrantStatusV1, MAX_GRANT_BYTES, grant_key};
use clap::Args;
use fs2::FileExt;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Args)]
pub struct PublishArgs {
    /// Complete ExecGrantV1 JSON; grant requires active, revoke requires revoked
    #[arg(long)]
    pub file: PathBuf,
    /// Pinned policy namespace public ID (64 lowercase hex characters)
    #[arg(long)]
    pub namespace: String,
    /// Owner-only file containing the namespace write secret as 64 hex characters
    #[arg(long)]
    pub namespace_secret_file: PathBuf,
    /// Expected audience; must equal the record and target service configuration
    #[arg(long)]
    pub audience: String,
    /// Policy replicas to reconcile before publication (required; every replica must sync)
    #[arg(long, required = true)]
    pub peer: Vec<String>,
}
pub(super) fn identity(value: &str) -> Result<[u8; 32]> {
    aster_exec::policy::parse_node_id(value)
        .ok_or_else(|| Error::usage("identity must be 64 lowercase hex characters"))
}
pub(super) use crate::files::read_bounded as bounded;
fn validate(grant: &ExecGrantV1, root: &str, audience: &str, revoked: bool) -> Result<Vec<u8>> {
    let caller = identity(&grant.caller)?;
    let target = identity(&grant.target)?;
    identity(&grant.issuer)?;
    let expected = if revoked {
        GrantStatusV1::Revoked
    } else {
        GrantStatusV1::Active
    };
    if grant.version != 1
        || grant.issuer != root
        || grant.audience != audience
        || audience.is_empty()
        || grant.revision == 0
        || grant.status != expected
        || !grant.cancel_on_revocation
        || grant.account_alias.is_empty()
        || grant.account_alias.len() > 64
        || !grant
            .account_alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || grant.profiles.is_empty()
        || grant.profiles.len() > 32
        || grant.operations.is_empty()
        || grant.operations.len() > 6
        || grant.cwd_roots.is_empty()
        || grant.cwd_roots.len() > 32
        || grant.cwd_roots.iter().any(|p| !Path::new(p).is_absolute())
        || grant.max_timeout_ms == 0
        || grant.max_output_bytes == 0
        || grant.issued_at > grant.expires_at.unwrap_or(u64::MAX)
        || grant
            .not_before
            .is_some_and(|nbf| grant.expires_at.is_some_and(|exp| nbf >= exp))
    {
        return Err(Error::usage(
            "grant context, status, scope or limits are invalid; supply an explicit v1 record matching the selected root, audience and operation",
        ));
    }
    Ok(grant_key(&caller, &target))
}
fn check_revision(old: Option<&ExecGrantV1>, new: &ExecGrantV1) -> Result<bool> {
    let Some(old) = old else { return Ok(true) };
    if old == new {
        return Ok(false);
    };
    if new.revision <= old.revision {
        return Err(Error::usage(
            "revision must increase; an existing revision may only be replayed with identical content",
        ));
    }
    if new.status == GrantStatusV1::Active
        && (old.status == GrantStatusV1::Revoked
            || old.expires_at.is_some_and(|exp| new.issued_at >= exp))
        && new.reauthorizes_revision != Some(old.revision)
    {
        return Err(Error::usage(
            "reactivation requires reauthorizes_revision equal to the previous denied revision",
        ));
    }
    Ok(true)
}

async fn write_grant(
    doc: &aster::Doc,
    author: &aster::AuthorId,
    key: &[u8],
    grant: &ExecGrantV1,
) -> Result<bool> {
    let previous = doc
        .get_exact(author, key.to_vec())
        .await?
        .map(|bytes| serde_json::from_slice::<ExecGrantV1>(&bytes))
        .transpose()?;
    if previous.is_none() && doc.entry_exists(author, key.to_vec()).await? {
        return Err(Error::new(
            ErrorKind::Denied,
            "root grant key has a deletion tombstone; restore an explicit revisioned denial before publishing",
        ));
    }
    let changed = check_revision(previous.as_ref(), grant)?;
    if changed {
        doc.set_bytes_durable(author, key.to_vec(), serde_json::to_vec(grant)?)
            .await?;
    }
    Ok(changed)
}

/// Synchronize with every policy replica, wait for the exact root record's
/// content to be readable, then write. Never reads the predecessor from a
/// possibly stale local replica.
async fn reconcile_and_write(
    doc: &aster::Doc,
    peers: &[aster::NodeId],
    author: &aster::AuthorId,
    key: &[u8],
    grant: &ExecGrantV1,
    budget: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    doc.sync_with_timeout(peers, budget).await?;
    // Metadata sync can finish before the record's content arrives.
    while let Err(error) = doc.get_exact(author, key.to_vec()).await {
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::Unavailable,
                format!("current root grant record is not yet readable: {error}"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    write_grant(doc, author, key, grant).await
}

pub async fn publish(ctx: &Ctx, args: PublishArgs, revoked: bool) -> Result<()> {
    let cfg = aster_config::load(&ctx.home)?;
    let profile = cfg.selected_profile_name(ctx.profile.as_deref());
    aster_config::validate_profile_name(&profile)?;
    let root = cfg
        .profiles
        .get(&profile)
        .and_then(|p| p.root_pubkey.as_ref())
        .ok_or_else(|| Error::new(ErrorKind::Denied, "selected profile has no root public key"))?;
    identity(root)?;
    let secret = aster_keystore::KeyStore::open(&ctx.home)?
        .get(&profile)?
        .ok_or_else(|| {
            Error::not_found(
                "selected profile has no root private key; import it with aster key root import",
            )
        })?;
    if secret.public_key_hex() != *root {
        return Err(Error::new(
            ErrorKind::Denied,
            "root private key does not match the selected profile",
        ));
    }
    let bytes = bounded(&args.file, MAX_GRANT_BYTES, false)?;
    let grant: ExecGrantV1 = serde_json::from_slice(&bytes)?;
    let key = validate(&grant, root, &args.audience, revoked)?;
    let namespace = identity(&args.namespace)?;
    let namespace_bytes = zeroize::Zeroizing::new(bounded(&args.namespace_secret_file, 128, true)?);
    let namespace_text = std::str::from_utf8(&namespace_bytes)
        .map_err(|_| Error::usage("namespace secret must contain hex"))?
        .trim();
    let namespace_secret = aster::NamespaceSecret::from_bytes(identity(namespace_text)?);
    if namespace_secret.id().to_bytes() != namespace {
        return Err(Error::new(
            ErrorKind::Denied,
            "namespace write secret does not match the pinned namespace",
        ));
    }
    let owner = ctx.home.join("exec-author").join(&profile);
    std::fs::create_dir_all(&owner)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::symlink_metadata(&owner)?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
            return Err(Error::new(
                ErrorKind::Denied,
                "invalid owner storage directory",
            ));
        }
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o700))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(owner.join("publish.lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        Error::new(
            ErrorKind::Unavailable,
            "another grant publisher owns this profile; retry after it completes",
        )
    })?;
    let settings = cfg
        .profiles
        .get(&profile)
        .ok_or_else(|| Error::usage("selected profile does not exist"))?;
    let credential_path = settings.exec_credential_file.as_ref().ok_or_else(|| {
        Error::usage(
            "publisher profile requires exec_credential_file for its enrolled node identity",
        )
    })?;
    if !credential_path.is_absolute() {
        return Err(Error::usage("exec_credential_file must be absolute"));
    }
    let credential =
        zeroize::Zeroizing::new(String::from_utf8(bounded(credential_path, 65536, true)?)?);
    let node_key = super::daemon::load_node_key(&super::daemon::node_key_path(ctx, &profile))?;
    // In memory: the root author is imported for this run only, and every
    // run reconciles with the replicas before reading anything.
    let config = aster::AsterConfig {
        secret_key: Some(node_key),
        root_pubkey: Some(aster::PublicKey::from_hex(root)?),
        enable_hooks: true,
        hook_failure_mode: aster::HookFailureMode::FailClosed,
        ..Default::default()
    };
    let node =
        aster::Node::start_with_alpns(config, vec![aster::enrollment::ENROLLMENT_ALPN.to_vec()])
            .await?;
    let enrollment = aster::enrollment::Enrollment::install(
        &node,
        std::sync::Arc::new(aster::enrollment::EnrollmentPolicyStore::open(
            owner.join("enrollment.json"),
            identity(root)?,
            namespace,
            &args.audience,
        )?),
        aster::enrollment::JwtEnrollmentVerifier::new(identity(root)?, &args.audience)?,
        Some(credential.trim().into()),
        std::sync::Arc::new(aster::enrollment::PolicyExpressionV1::Predicate(
            aster::enrollment::PolicyPredicate::TolerateStalePolicy,
        )),
        aster::enrollment::EnrollmentOptions::default(),
    )?;
    let peer_ids: std::collections::BTreeSet<_> = args.peer.iter().cloned().collect();
    for peer in &peer_ids {
        identity(peer)?;
        tokio::time::timeout(
            Duration::from_secs(15),
            enrollment.connect(aster::NodeAddr {
                node_id: aster::NodeId::from_hex(peer),
                relay_url: None,
                direct_addresses: settings
                    .exec_direct_addresses
                    .get(peer)
                    .cloned()
                    .unwrap_or_default(),
            }),
        )
        .await
        .map_err(|_| {
            Error::new(
                ErrorKind::Unavailable,
                "publisher enrollment deadline exceeded",
            )
        })??;
    }
    let docs = node.docs();
    docs.native_docs()
        .api()
        .author_import(aster::native::iroh_docs::Author::from_bytes(
            secret.as_bytes(),
        ))
        .await?;
    let author = aster::AuthorId::from_hex(root.clone());
    let doc = docs
        .open_or_import_write_namespace(namespace_secret)
        .await?;
    let peers: Vec<aster::NodeId> = args
        .peer
        .iter()
        .map(|p| identity(p).map(|_| aster::NodeId::from_hex(p)))
        .collect::<Result<_>>()?;
    let changed =
        reconcile_and_write(&doc, &peers, &author, &key, &grant, Duration::from_secs(15)).await?;
    let read_ticket = doc.share_with_addr(aster::ShareMode::Read).await?;
    // The write exists only in this in-memory replica until delivered, so an
    // undelivered publication fails. Rerunning the same command is an
    // idempotent replay that retries delivery.
    let delivered = doc
        .sync_with_timeout(&peers, Duration::from_secs(15))
        .await
        .is_ok();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::usage("system clock precedes Unix epoch"))?
        .as_secs();
    let report = json!({
        "written": changed,
        "delivered": delivered,
        "namespace": args.namespace,
        "author": root,
        "key": String::from_utf8(key)?,
        "revision": grant.revision,
        "status": grant.status,
        "read_ticket": read_ticket,
        "replicas": args.peer,
        "publisher_node_id": node.id().to_string(),
        "observed_at": now,
        "note": "Delivery to replicas does not prove target authorization or cancellation; run exec check/status separately.",
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    doc.leave().await?;
    drop(doc);
    drop(docs);
    node.shutdown().await;
    if !delivered {
        return Err(Error::new(
            ErrorKind::Unavailable,
            "grant was not delivered to every policy replica; rerun the same command to retry",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grant() -> ExecGrantV1 {
        serde_json::from_value(json!({"version":1,"issuer":"11".repeat(32),"audience":"fleet","caller":"22".repeat(32),"target":"33".repeat(32),"revision":1,"issued_at":1,"status":"active","account_alias":"builder","profiles":["open"],"operations":["submit"],"cwd_roots":["/srv/build"],"max_timeout_ms":1000,"max_output_bytes":4096,"cancel_on_revocation":true})).unwrap()
    }
    #[tokio::test]
    async fn publishes_exact_root_record_durably_and_replays_idempotently() {
        let node = aster::Node::start(aster::AsterConfig::builder().build())
            .await
            .unwrap();
        let docs = node.docs();
        let doc = docs.create().await.unwrap();
        let root = aster::SecretKey::generate();
        docs.native_docs()
            .api()
            .author_import(aster::native::iroh_docs::Author::from_bytes(
                &root.to_bytes(),
            ))
            .await
            .unwrap();
        let author = aster::default_author_id(&root);
        let mut g = grant();
        g.issuer = author.to_string();
        let key = validate(&g, &g.issuer, "fleet", false).unwrap();
        assert!(write_grant(&doc, &author, &key, &g).await.unwrap());
        assert!(!write_grant(&doc, &author, &key, &g).await.unwrap());
        let stored: ExecGrantV1 =
            serde_json::from_slice(&doc.get_exact(&author, key.clone()).await.unwrap().unwrap())
                .unwrap();
        assert_eq!(stored, g);
        let unrelated = docs.default_author().await.unwrap();
        doc.set_bytes(&unrelated, key.clone(), b"spoof".to_vec())
            .await
            .unwrap();
        assert!(!write_grant(&doc, &author, &key, &g).await.unwrap());
        g.status = GrantStatusV1::Revoked;
        g.revision = 2;
        assert!(write_grant(&doc, &author, &key, &g).await.unwrap());
        doc.del(&author, key.clone()).await.unwrap();
        g.revision = 3;
        assert!(write_grant(&doc, &author, &key, &g).await.is_err());
        doc.leave().await.unwrap();
        drop(doc);
        drop(docs);
        node.shutdown().await;
    }
    #[tokio::test]
    async fn stale_publisher_reconciles_exact_root_revocation_before_writing() {
        let config = || {
            aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .build()
        };
        let replica = aster::Node::start(config()).await.unwrap();
        let publisher = aster::Node::start(config()).await.unwrap();
        for node in [&replica, &publisher] {
            tokio::time::timeout(Duration::from_secs(5), async {
                while node.addr().direct_addresses.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        publisher.add_peer(&replica).unwrap();
        replica.add_peer(&publisher).unwrap();
        let namespace = aster::NamespaceSecret::from_bytes([45; 32]);
        let remote = replica
            .docs()
            .open_or_import_write_namespace(namespace.clone())
            .await
            .unwrap();
        let local = publisher
            .docs()
            .open_or_import_write_namespace(namespace)
            .await
            .unwrap();
        let root = aster::SecretKey::generate();
        for node in [&replica, &publisher] {
            node.docs()
                .native_docs()
                .api()
                .author_import(aster::native::iroh_docs::Author::from_bytes(
                    &root.to_bytes(),
                ))
                .await
                .unwrap();
        }
        let author = aster::default_author_id(&root);
        let mut denied = grant();
        denied.issuer = author.to_string();
        denied.revision = 7;
        denied.status = GrantStatusV1::Revoked;
        let key = validate(&denied, &denied.issuer, "fleet", true).unwrap();
        write_grant(&remote, &author, &key, &denied).await.unwrap();
        assert!(
            local
                .get_exact(&author, key.clone())
                .await
                .unwrap()
                .is_none()
        );
        // A newer unrelated author's active record must never replace the root floor.
        let unrelated = replica.docs().default_author().await.unwrap();
        let mut stale = denied.clone();
        stale.revision = 1;
        stale.status = GrantStatusV1::Active;
        remote
            .set_bytes(&unrelated, key.clone(), serde_json::to_vec(&stale).unwrap())
            .await
            .unwrap();
        // Importing a namespace does not enable the replica's sync handler.
        remote.start_sync(&[]).await.unwrap();
        // The stale publisher learns revision 7 before writing and refuses
        // its own lower revision.
        let refused = reconcile_and_write(
            &local,
            &[replica.id()],
            &author,
            &key,
            &stale,
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
        assert!(
            refused.to_string().contains("revision must increase"),
            "{refused}"
        );
        let actual: ExecGrantV1 =
            serde_json::from_slice(&local.get_exact(&author, key).await.unwrap().unwrap()).unwrap();
        assert_eq!(actual, denied);
        remote.leave().await.unwrap();
        local.leave().await.unwrap();
        drop(remote);
        drop(local);
        publisher.shutdown().await;
        replica.shutdown().await;
    }

    #[tokio::test]
    async fn unavailable_replica_or_missing_peers_never_publish() {
        let node = aster::Node::start(
            aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .build(),
        )
        .await
        .unwrap();
        let doc = node.docs().create().await.unwrap();
        let author = node.docs().default_author().await.unwrap();
        let mut g = grant();
        g.issuer = author.to_string();
        let key = validate(&g, &g.issuer, "fleet", false).unwrap();
        assert!(
            reconcile_and_write(&doc, &[], &author, &key, &g, Duration::from_millis(20))
                .await
                .is_err()
        );
        let missing = aster::NodeId::from_hex(
            aster::default_author_id(&aster::SecretKey::generate()).to_string(),
        );
        assert!(
            reconcile_and_write(
                &doc,
                &[missing],
                &author,
                &key,
                &g,
                Duration::from_millis(50)
            )
            .await
            .is_err()
        );
        assert!(doc.get_exact(&author, key).await.unwrap().is_none());
        doc.leave().await.unwrap();
        drop(doc);
        node.shutdown().await;
    }

    #[test]
    fn publication_is_bound_to_root_context_and_explicit_status() {
        let g = grant();
        assert!(validate(&g, &g.issuer, "fleet", false).is_ok());
        assert!(validate(&g, &"44".repeat(32), "fleet", false).is_err());
        assert!(validate(&g, &g.issuer, "other", false).is_err());
        assert!(validate(&g, &g.issuer, "fleet", true).is_err());
    }
    #[test]
    fn revisions_are_idempotent_and_revocation_needs_explicit_reauthorization() {
        let g = grant();
        assert!(!check_revision(Some(&g), &g).unwrap());
        let mut revoked = g.clone();
        revoked.status = GrantStatusV1::Revoked;
        assert!(check_revision(Some(&g), &revoked).is_err());
        revoked.revision = 2;
        assert!(check_revision(Some(&g), &revoked).unwrap());
        let mut active = g;
        active.revision = 3;
        assert!(check_revision(Some(&revoked), &active).is_err());
        active.reauthorizes_revision = Some(2);
        assert!(check_revision(Some(&revoked), &active).unwrap());
    }
}
