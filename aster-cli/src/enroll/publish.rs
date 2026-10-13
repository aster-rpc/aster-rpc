//! A root publication session: the root's online window (enrollment §4).
//!
//! An in-memory node enrolls with the named policy replicas using the
//! profile's node identity, imports the root's Docs author for this run only,
//! reconciles each namespace with every replica before reading anything, and
//! fails unless what it wrote reached every replica. Rerunning a command is an
//! idempotent replay that retries delivery.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aster::enrollment::{
    ENROLLMENT_ALPN, Enrollment, EnrollmentOptions, EnrollmentPolicyStore, JwtEnrollmentVerifier,
    PolicyExpressionV1, PolicyPredicate,
};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use clap::Args;
use fs2::FileExt;

use crate::files::read_bounded;

/// Replicas download a record's content after its metadata; keep serving
/// briefly after delivery, since the session cannot observe the downloads.
const CONTENT_SERVE_GRACE: Duration = Duration::from_secs(5);
const SYNC_BUDGET: Duration = Duration::from_secs(15);

#[derive(Debug, Args)]
pub struct FleetArgs {
    /// Fleet namespace public ID (64 lowercase hex characters)
    #[arg(long)]
    pub fleet_namespace: String,
    /// Owner-only file containing the fleet namespace write secret (64 hex characters)
    #[arg(long, value_name = "FILE")]
    pub fleet_secret_file: PathBuf,
    /// Fleet audience; records name it and verifiers pin it
    #[arg(long)]
    pub audience: String,
    /// Policy replica to reconcile with before writing and deliver to after; repeatable
    #[arg(long, required = true)]
    pub peer: Vec<String>,
}

pub(super) struct Session {
    node: aster::Node,
    _enrollment: Enrollment,
    _lock: std::fs::File,
    root: [u8; 32],
    author: aster::AuthorId,
    peers: Vec<aster::NodeId>,
    audience: String,
    fleet_namespace: [u8; 32],
}

pub(super) fn hex32(value: &str, what: &str) -> Result<[u8; 32]> {
    let bytes: [u8; 32] = hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| Error::usage(format!("{what} must be 64 lowercase hex characters")))?;
    if hex::encode(bytes) != value {
        return Err(Error::usage(format!(
            "{what} must be 64 lowercase hex characters"
        )));
    }
    Ok(bytes)
}

/// Read an owner-only namespace write secret and check it names `namespace`.
pub(super) fn namespace_secret(
    path: &Path,
    namespace: &[u8; 32],
) -> Result<aster::NamespaceSecret> {
    let bytes = zeroize::Zeroizing::new(read_bounded(path, 128, true)?);
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::usage("namespace secret must contain hex"))?
        .trim();
    let secret = aster::NamespaceSecret::from_bytes(hex32(text, "namespace secret")?);
    if secret.id().to_bytes() != *namespace {
        return Err(Error::new(
            ErrorKind::Denied,
            "namespace write secret does not match the namespace",
        ));
    }
    Ok(secret)
}

impl Session {
    pub fn root(&self) -> &[u8; 32] {
        &self.root
    }
    pub fn audience(&self) -> &str {
        &self.audience
    }
    pub fn fleet_namespace(&self) -> &[u8; 32] {
        &self.fleet_namespace
    }

    /// Start the session as the selected profile's root.
    pub async fn start(ctx: &Ctx, fleet: &FleetArgs) -> Result<Self> {
        Self::start_as(
            ctx,
            &fleet.fleet_namespace,
            &fleet.audience,
            &fleet.peer,
            None,
        )
        .await
    }

    /// Start the session writing as `author` (an observer's key) instead of
    /// the root; the root's private key is not needed.
    pub async fn start_as(
        ctx: &Ctx,
        fleet_namespace: &str,
        audience: &str,
        peer: &[String],
        author: Option<zeroize::Zeroizing<[u8; 32]>>,
    ) -> Result<Self> {
        let cfg = aster_config::load(&ctx.home)?;
        let profile = cfg.selected_profile_name(ctx.profile.as_deref());
        aster_config::validate_profile_name(&profile)?;
        let settings = cfg
            .profiles
            .get(&profile)
            .ok_or_else(|| Error::usage("selected profile does not exist"))?;
        let root_hex = settings.root_pubkey.as_ref().ok_or_else(|| {
            Error::new(ErrorKind::Denied, "selected profile has no root public key")
        })?;
        let root = hex32(root_hex, "profile root public key")?;
        let author_secret = match author {
            Some(secret) => secret,
            None => {
                let secret = aster_keystore::KeyStore::open(&ctx.home)?
                    .get(&profile)?
                    .ok_or_else(|| {
                        Error::not_found(
                            "selected profile has no root private key; import it with aster key root import",
                        )
                    })?;
                if secret.public_key_hex() != *root_hex {
                    return Err(Error::new(
                        ErrorKind::Denied,
                        "root private key does not match the selected profile",
                    ));
                }
                zeroize::Zeroizing::new(*secret.as_bytes())
            }
        };
        let author_public =
            aster::enrollment::JwtEnrollmentIssuer::from_seed(&author_secret).public_key();
        let fleet_namespace = hex32(fleet_namespace, "fleet namespace")?;
        let owner = ctx.home.join("enroll-publisher").join(&profile);
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
                "another root publication owns this profile; retry after it completes",
            )
        })?;
        let credential_path = settings.exec_credential_file.as_ref().ok_or_else(|| {
            Error::usage(
                "publisher profile requires exec_credential_file for its enrolled node identity",
            )
        })?;
        if !credential_path.is_absolute() {
            return Err(Error::usage("exec_credential_file must be absolute"));
        }
        let credential = zeroize::Zeroizing::new(String::from_utf8(read_bounded(
            credential_path,
            65536,
            true,
        )?)?);
        let node_key = {
            let path = ctx.home.join("keys").join(format!("{profile}.node.key"));
            let bytes = zeroize::Zeroizing::new(read_bounded(&path, 128, true)?);
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| Error::usage("node key must contain hex"))?;
            aster::SecretKey::from_bytes(hex32(text.trim(), "node key")?)
        };
        let config = aster::AsterConfig {
            secret_key: Some(node_key),
            root_pubkey: Some(aster::PublicKey::from_hex(root_hex)?),
            enable_hooks: true,
            hook_failure_mode: aster::HookFailureMode::FailClosed,
            ..Default::default()
        };
        let node = aster::Node::start_with_alpns(config, vec![ENROLLMENT_ALPN.to_vec()]).await?;
        let enrollment = Enrollment::install(
            &node,
            Arc::new(EnrollmentPolicyStore::open(
                owner.join("enrollment.json"),
                root,
                fleet_namespace,
                audience,
            )?),
            JwtEnrollmentVerifier::new(root, audience)?,
            Some(credential.trim().into()),
            Arc::new(PolicyExpressionV1::Predicate(
                PolicyPredicate::TolerateStalePolicy,
            )),
            EnrollmentOptions::default(),
        )?;
        let mut peers = Vec::new();
        for peer in peer.iter().collect::<std::collections::BTreeSet<_>>() {
            hex32(peer, "peer")?;
            tokio::time::timeout(
                SYNC_BUDGET,
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
            peers.push(aster::NodeId::from_hex(peer));
        }
        node.docs()
            .native_docs()
            .api()
            .author_import(aster::native::iroh_docs::Author::from_bytes(&author_secret))
            .await?;
        Ok(Self {
            author: aster::AuthorId::from_hex(hex::encode(author_public)),
            node,
            _enrollment: enrollment,
            _lock: lock,
            root,
            peers,
            audience: audience.to_owned(),
            fleet_namespace,
        })
    }

    /// Open a namespace this session writes as the root, then reconcile it
    /// with every replica so later reads see the current records.
    pub async fn writable(&self, namespace: &[u8; 32], secret_file: &Path) -> Result<aster::Doc> {
        let secret = namespace_secret(secret_file, namespace)?;
        let doc = self
            .node
            .docs()
            .open_or_import_write_namespace(secret)
            .await?;
        doc.sync_with_timeout(&self.peers, SYNC_BUDGET).await?;
        Ok(doc)
    }

    /// The root's current record at `key`, waiting for content the metadata
    /// sync named. Never reads another author's entry.
    pub async fn read_root(&self, doc: &aster::Doc, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let deadline = tokio::time::Instant::now() + SYNC_BUDGET;
        loop {
            match doc.get_exact(&self.author, key.to_vec()).await {
                Ok(value) => return Ok(value),
                Err(error) if tokio::time::Instant::now() >= deadline => {
                    return Err(Error::new(
                        ErrorKind::Unavailable,
                        format!("current root record is not yet readable: {error}"),
                    ));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    /// Write a root record durably. Refuses a key the root deleted, which
    /// would otherwise hide a denial.
    pub async fn put_root(&self, doc: &aster::Doc, key: &[u8], value: Vec<u8>) -> Result<()> {
        if self.read_root(doc, key).await?.is_none()
            && doc.entry_exists(&self.author, key.to_vec()).await?
        {
            return Err(Error::new(
                ErrorKind::Denied,
                "root record key has a deletion tombstone; publish an explicit revisioned record instead",
            ));
        }
        doc.set_bytes_durable(&self.author, key.to_vec(), value)
            .await?;
        Ok(())
    }

    /// Leave read-only namespaces and shut down without delivering anything.
    pub async fn close(self, docs: Vec<aster::Doc>) -> Result<()> {
        for doc in docs {
            doc.leave().await?;
        }
        self.node.shutdown().await;
        Ok(())
    }

    /// Where this session keeps local state, such as the review store.
    pub fn state_path(ctx: &Ctx, name: &str) -> Result<PathBuf> {
        let cfg = aster_config::load(&ctx.home)?;
        let profile = cfg.selected_profile_name(ctx.profile.as_deref());
        Ok(ctx.home.join("enroll-publisher").join(profile).join(name))
    }

    /// Deliver every written namespace to every replica, keep serving
    /// briefly, and shut down. Fails if any delivery failed.
    pub async fn finish(self, docs: Vec<aster::Doc>) -> Result<()> {
        let mut delivered = true;
        for doc in &docs {
            delivered &= doc
                .sync_with_timeout(&self.peers, SYNC_BUDGET)
                .await
                .is_ok();
        }
        if delivered {
            tokio::time::sleep(CONTENT_SERVE_GRACE).await;
        }
        for doc in docs {
            doc.leave().await?;
        }
        self.node.shutdown().await;
        if !delivered {
            return Err(Error::new(
                ErrorKind::Unavailable,
                "records were not delivered to every policy replica; rerun the same command to retry",
            ));
        }
        Ok(())
    }
}

/// A root record's revision may only increase; an equal revision must be an
/// identical replay. Returns whether anything needs writing.
pub(super) fn check_revision(
    previous: Option<(u64, Vec<u8>)>,
    revision: u64,
    bytes: &[u8],
) -> Result<bool> {
    match previous {
        None => Ok(true),
        Some((old, old_bytes)) if old == revision && old_bytes == bytes => Ok(false),
        Some((old, _)) if revision > old => Ok(true),
        Some(_) => Err(Error::usage(
            "revision must increase; an existing revision may only be replayed with identical content",
        )),
    }
}
