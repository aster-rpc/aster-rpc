//! Root-authored fleet records: namespace bindings (B9), sealed observer
//! write grants (D2), issuer status (B8) and the issuer-key publishers'
//! sealed fleet write grants (gateway identity). Semantics live in Aster
//! core; this adapter builds the records and publishes them as the root.

use std::path::PathBuf;

use aster::enrollment::{
    Audience, BindingStatus, IssuerStatus, IssuerStatusV1, NamespaceBindingV1, NamespacePurpose,
    OBSERVER_GRANT_APP, OBSERVER_GRANT_PREFIX, issuer_status_key, namespace_binding_key,
    publisher_grant_key, publisher_grant_record, publisher_grant_withdrawal,
};
use aster::grants::{GrantContext, GrantRecord, grant_key, seal_namespace_grant};
use aster_cli_kit::out::{Report, Styler};
use aster_cli_kit::{Ctx, Error, Result};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use super::publish::{FleetArgs, Session, check_revision, hex32, namespace_secret};

#[derive(Debug, Subcommand)]
pub enum NamespaceCommand {
    /// Generate a new policy namespace and its owner-only write secret file
    Create(CreateArgs),
    /// Bind a namespace to the fleet (or retire it) as the root
    Bind(Box<BindArgs>),
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// New owner-only secret file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out_secret: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Purpose {
    Membership,
    ExecGrants,
    Observations,
}

impl From<Purpose> for NamespacePurpose {
    fn from(value: Purpose) -> Self {
        match value {
            Purpose::Membership => Self::Membership,
            Purpose::ExecGrants => Self::ExecGrants,
            Purpose::Observations => Self::Observations,
        }
    }
}

#[derive(Debug, Args)]
pub struct BindArgs {
    /// The bound namespace's public ID (64 lowercase hex characters)
    #[arg(long)]
    namespace: String,
    #[arg(long, value_enum)]
    purpose: Purpose,
    /// Nodes whose verified scopes include this one receive a read ticket; repeatable
    #[arg(long)]
    read_scope: Vec<String>,
    /// Observer scope allowed to write observations; repeatable (observations only)
    #[arg(long)]
    write_scope: Vec<String>,
    /// Binding revision; must exceed the published one
    #[arg(long)]
    revision: u64,
    /// Retire the binding; final for this namespace ID
    #[arg(long)]
    retire: bool,
    /// Seal the observations write capability to this observer key; repeatable
    #[arg(long, value_name = "KEY", requires = "namespace_secret_file")]
    seal_to: Vec<String>,
    /// Withdraw a sealed observer grant from this key; repeatable
    #[arg(long, value_name = "KEY")]
    unseal: Vec<String>,
    /// Owner-only file with the bound namespace's write secret (for --seal-to)
    #[arg(long, value_name = "FILE")]
    namespace_secret_file: Option<PathBuf>,
    #[command(flatten)]
    fleet: FleetArgs,
}

#[derive(Debug, Args)]
pub struct IssuerArgs {
    /// The delegate (online issuer) public key
    #[arg(long)]
    delegate: String,
    /// Status revision; must exceed the published one
    #[arg(long)]
    revision: u64,
    /// Reactivate a revoked issuer instead of revoking it; needs --reauthorizes
    #[arg(long, requires = "reauthorizes")]
    reactivate: bool,
    /// The exact revoked revision a reactivation names
    #[arg(long)]
    reauthorizes: Option<u64>,
    #[command(flatten)]
    fleet: FleetArgs,
}

#[derive(Debug, Args)]
pub struct PublisherArgs {
    /// Seal the fleet namespace's write capability to this publisher's node key; repeatable.
    /// Its credential must also grant `aster.identity-publisher`
    #[arg(long, value_name = "KEY", required_unless_present = "unseal")]
    seal_to: Vec<String>,
    /// Withdraw a publisher's grant; repeatable
    #[arg(long, value_name = "KEY")]
    unseal: Vec<String>,
    #[command(flatten)]
    fleet: FleetArgs,
}

#[derive(Serialize)]
struct PublisherGrantReport {
    sealed_to: Vec<String>,
    unsealed: Vec<String>,
    delivered: bool,
}

impl Report for PublisherGrantReport {
    fn human(&self, _: Styler) -> String {
        let mut text = String::from("Delivered to every policy replica.");
        if !self.sealed_to.is_empty() {
            text.push_str(&format!(
                "\nSealed identity-publisher grants: {}",
                self.sealed_to.join(", ")
            ));
        }
        if !self.unsealed.is_empty() {
            text.push_str(&format!(
                "\nWithdrew identity-publisher grants: {}",
                self.unsealed.join(", ")
            ));
        }
        text
    }
}

#[derive(Serialize)]
struct CreatedReport {
    namespace: String,
    secret_file: PathBuf,
}

impl Report for CreatedReport {
    fn human(&self, _: Styler) -> String {
        format!(
            "Created namespace {}\nWrite secret: {}\nBind it with `aster enroll namespace bind`.",
            self.namespace,
            self.secret_file.display()
        )
    }
}

#[derive(Serialize)]
struct PublishedReport {
    record: &'static str,
    key: String,
    revision: u64,
    written: bool,
    sealed_to: Vec<String>,
    unsealed: Vec<String>,
    delivered: bool,
}

impl Report for PublishedReport {
    fn human(&self, _: Styler) -> String {
        let action = if self.written {
            "Published"
        } else {
            "Already published"
        };
        let mut text = format!(
            "{action} {} revision {} at {}\nDelivered to every policy replica.",
            self.record, self.revision, self.key
        );
        if !self.sealed_to.is_empty() {
            text.push_str(&format!(
                "\nSealed observer grants: {}",
                self.sealed_to.join(", ")
            ));
        }
        if !self.unsealed.is_empty() {
            text.push_str(&format!(
                "\nWithdrew observer grants: {}",
                self.unsealed.join(", ")
            ));
        }
        text
    }
}

pub async fn namespace(ctx: &Ctx, command: NamespaceCommand) -> Result<()> {
    match command {
        NamespaceCommand::Create(args) => ctx.emit(&create(&args)?),
        NamespaceCommand::Bind(args) => ctx.emit(&bind(ctx, *args).await?),
    }
}

fn create(args: &CreateArgs) -> Result<CreatedReport> {
    let secret = aster::NamespaceSecret::from_bytes(aster::SecretKey::generate().to_bytes());
    let namespace = hex::encode(secret.id().to_bytes());
    let text = zeroize::Zeroizing::new(hex::encode(secret.to_bytes()));
    super::write_new(&args.out_secret, &namespace, &text)?;
    Ok(CreatedReport {
        namespace,
        secret_file: args.out_secret.clone(),
    })
}

/// Check a binding as verifiers will: the fleet cannot bind itself, only
/// the observations namespace names writer scopes, and it is never shared by
/// read ticket.
pub(super) fn validate_binding(record: &NamespaceBindingV1, fleet: &[u8; 32]) -> Result<()> {
    if record.namespace == hex::encode(fleet) {
        return Err(Error::usage("the fleet namespace cannot bind itself"));
    }
    if record.revision == 0 {
        return Err(Error::usage("revision must be positive"));
    }
    let observations = record.purpose == NamespacePurpose::Observations;
    if !observations && !record.write_scopes.is_empty() {
        return Err(Error::usage(
            "--write-scope is only for the observations namespace",
        ));
    }
    if observations && !record.read_scopes.is_empty() {
        return Err(Error::usage(
            "the observations namespace is never shared by read ticket; omit --read-scope",
        ));
    }
    Ok(())
}

/// The sealed grant record for one observer, and the key it is written at.
pub(super) fn observer_grant(
    root: &[u8; 32],
    observer: &[u8; 32],
    observations: &aster::NamespaceSecret,
) -> Result<(String, Vec<u8>)> {
    let granter = aster::NodeId::from_hex(hex::encode(root));
    let recipient = aster::NodeId::from_hex(hex::encode(observer));
    let key = grant_key(OBSERVER_GRANT_PREFIX, &recipient);
    let resource = observations.id().to_bytes();
    let context = GrantContext {
        app: OBSERVER_GRANT_APP,
        granter: &granter,
        recipient: &recipient,
        resource: &resource,
        path: &key,
        role: "write",
    };
    let envelope = seal_namespace_grant(
        &context,
        &aster::NamespaceCapability::write(observations.clone()),
    )?;
    Ok((key, GrantRecord::active("write", 1, envelope).encode_fory()))
}

fn previous_revision<T: serde::de::DeserializeOwned>(
    bytes: Option<Vec<u8>>,
    revision: impl Fn(&T) -> u64,
) -> Result<Option<(u64, Vec<u8>)>> {
    bytes
        .map(|bytes| {
            let record: T = serde_json::from_slice(&bytes).map_err(|_| {
                Error::usage("the published record is unreadable; inspect it first")
            })?;
            Ok((revision(&record), bytes))
        })
        .transpose()
}

async fn bind(ctx: &Ctx, args: BindArgs) -> Result<PublishedReport> {
    let namespace = hex32(&args.namespace, "namespace")?;
    let purpose = NamespacePurpose::from(args.purpose);
    if (!args.seal_to.is_empty() || !args.unseal.is_empty())
        && purpose != NamespacePurpose::Observations
    {
        return Err(Error::usage(
            "--seal-to and --unseal apply only to the observations namespace",
        ));
    }
    let observations = match &args.namespace_secret_file {
        Some(path) => Some(namespace_secret(path, &namespace)?),
        None => None,
    };
    let seal_to = args
        .seal_to
        .iter()
        .map(|key| hex32(key, "observer key"))
        .collect::<Result<Vec<_>>>()?;
    let unseal = args
        .unseal
        .iter()
        .map(|key| hex32(key, "observer key"))
        .collect::<Result<Vec<_>>>()?;
    let session = Session::start(ctx, &args.fleet).await?;
    let record = NamespaceBindingV1 {
        version: 1,
        issuer: hex::encode(session.root()),
        namespace: hex::encode(namespace),
        purpose,
        read_scopes: args.read_scope.into_iter().collect(),
        write_scopes: args.write_scope.into_iter().collect(),
        revision: args.revision,
        status: if args.retire {
            BindingStatus::Retired
        } else {
            BindingStatus::Active
        },
    };
    validate_binding(&record, session.fleet_namespace())?;
    let fleet = session
        .writable(session.fleet_namespace(), &args.fleet.fleet_secret_file)
        .await?;
    let key = namespace_binding_key(&namespace);
    let bytes = serde_json::to_vec(&record)?;
    let previous = previous_revision(
        session.read_root(&fleet, &key).await?,
        |old: &NamespaceBindingV1| old.revision,
    )?;
    let written = check_revision(previous, record.revision, &bytes)?;
    if written {
        session.put_root(&fleet, &key, bytes).await?;
    }
    for observer in &seal_to {
        let (grant_key, grant) = observer_grant(
            session.root(),
            observer,
            observations.as_ref().ok_or_else(|| {
                Error::usage(
                    "--seal-to needs --namespace-secret-file for the observations namespace",
                )
            })?,
        )?;
        session
            .put_root(&fleet, grant_key.as_bytes(), grant)
            .await?;
    }
    for observer in &unseal {
        let recipient = aster::NodeId::from_hex(hex::encode(observer));
        let grant_key = grant_key(OBSERVER_GRANT_PREFIX, &recipient);
        let revoked = GrantRecord::revoked("write", "observer withdrawn").encode_fory();
        session
            .put_root(&fleet, grant_key.as_bytes(), revoked)
            .await?;
    }
    session.finish(vec![fleet]).await?;
    Ok(PublishedReport {
        record: "namespace binding",
        key: String::from_utf8_lossy(&key).into_owned(),
        revision: record.revision,
        written,
        sealed_to: seal_to.iter().map(hex::encode).collect(),
        unsealed: unseal.iter().map(hex::encode).collect(),
        delivered: true,
    })
}

pub(super) fn issuer_record(
    root: &[u8; 32],
    delegate: &[u8; 32],
    audience: &str,
    revision: u64,
    issued_at: u64,
    reactivate: bool,
    reauthorizes: Option<u64>,
) -> Result<IssuerStatusV1> {
    if delegate == root {
        return Err(Error::usage("the root is not a delegate"));
    }
    if revision == 0 {
        return Err(Error::usage("revision must be positive"));
    }
    if reactivate && reauthorizes.is_none_or(|denied| denied >= revision) {
        return Err(Error::usage(
            "--reactivate must name the revoked revision with --reauthorizes, below --revision",
        ));
    }
    Ok(IssuerStatusV1 {
        version: 1,
        issuer: hex::encode(root),
        subject: hex::encode(delegate),
        audience: Audience::One(audience.to_owned()),
        revision,
        issued_at,
        status: if reactivate {
            IssuerStatus::Active
        } else {
            IssuerStatus::Revoked
        },
        reauthorizes_revision: if reactivate { reauthorizes } else { None },
    })
}

pub async fn issuer(ctx: &Ctx, args: IssuerArgs, now: u64) -> Result<()> {
    let delegate = hex32(&args.delegate, "delegate")?;
    let session = Session::start(ctx, &args.fleet).await?;
    let record = issuer_record(
        session.root(),
        &delegate,
        session.audience(),
        args.revision,
        now,
        args.reactivate,
        args.reauthorizes,
    )?;
    let fleet = session
        .writable(session.fleet_namespace(), &args.fleet.fleet_secret_file)
        .await?;
    let key = issuer_status_key(&delegate);
    let previous = session.read_root(&fleet, &key).await?;
    // Replaying the same revision keeps the published issuance time.
    let previous = previous_revision(previous, |old: &IssuerStatusV1| old.revision)?;
    let bytes = match &previous {
        Some((old, old_bytes)) if *old == record.revision => {
            let published: IssuerStatusV1 = serde_json::from_slice(old_bytes)?;
            let replay = IssuerStatusV1 {
                issued_at: published.issued_at,
                ..record.clone()
            };
            if replay != published {
                return Err(Error::usage(
                    "revision must increase; an existing revision may only be replayed with identical content",
                ));
            }
            old_bytes.clone()
        }
        _ => serde_json::to_vec(&record)?,
    };
    let written = check_revision(previous, record.revision, &bytes)?;
    if written {
        session.put_root(&fleet, &key, bytes).await?;
    }
    session.finish(vec![fleet]).await?;
    ctx.emit(&PublishedReport {
        record: "issuer status",
        key: String::from_utf8_lossy(&key).into_owned(),
        revision: record.revision,
        written,
        sealed_to: vec![],
        unsealed: vec![],
        delivered: true,
    })
}

/// Seal the fleet namespace's write grant to issuer-key publishers, or
/// withdraw it, as the root.
pub async fn identity_publisher(ctx: &Ctx, args: PublisherArgs) -> Result<()> {
    let seal_to = args
        .seal_to
        .iter()
        .map(|key| hex32(key, "publisher key"))
        .collect::<Result<Vec<_>>>()?;
    let unseal = args
        .unseal
        .iter()
        .map(|key| hex32(key, "publisher key"))
        .collect::<Result<Vec<_>>>()?;
    if seal_to.iter().any(|key| unseal.contains(key)) {
        return Err(Error::usage("a key cannot be both sealed to and unsealed"));
    }
    let session = Session::start(ctx, &args.fleet).await?;
    let secret = namespace_secret(&args.fleet.fleet_secret_file, session.fleet_namespace())?;
    let fleet = session
        .writable(session.fleet_namespace(), &args.fleet.fleet_secret_file)
        .await?;
    for publisher in &seal_to {
        let (key, record) =
            publisher_grant_record(session.root(), publisher, &secret.to_bytes())
                .map_err(|e| Error::usage(format!("sealing the publisher grant: {e:#}")))?;
        session.put_root(&fleet, key.as_bytes(), record).await?;
    }
    for publisher in &unseal {
        let key = publisher_grant_key(publisher);
        session
            .put_root(
                &fleet,
                key.as_bytes(),
                publisher_grant_withdrawal("identity publisher withdrawn"),
            )
            .await?;
    }
    session.finish(vec![fleet]).await?;
    ctx.emit(&PublisherGrantReport {
        sealed_to: seal_to.iter().map(hex::encode).collect(),
        unsealed: unseal.iter().map(hex::encode).collect(),
        delivered: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> [u8; 32] {
        aster::enrollment::JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
    }

    #[test]
    fn binding_records_follow_the_verifier_rules() {
        let scopes = |values: &[&str]| values.iter().map(|v| v.to_string()).collect();
        let record = |purpose, read: &[&str], write: &[&str], revision| NamespaceBindingV1 {
            version: 1,
            issuer: hex::encode(key(1)),
            namespace: hex::encode([2; 32]),
            purpose,
            read_scopes: scopes(read),
            write_scopes: scopes(write),
            revision,
            status: BindingStatus::Active,
        };
        let fleet = [1; 32];
        assert!(
            validate_binding(
                &record(NamespacePurpose::Membership, &["build"], &[], 3),
                &fleet
            )
            .is_ok()
        );
        assert!(
            validate_binding(
                &record(NamespacePurpose::Observations, &[], &["observer"], 1),
                &fleet
            )
            .is_ok()
        );
        assert!(
            validate_binding(&record(NamespacePurpose::Membership, &[], &[], 1), &[2; 32]).is_err()
        );
        assert!(
            validate_binding(
                &record(NamespacePurpose::Membership, &[], &["observer"], 1),
                &fleet
            )
            .is_err()
        );
        assert!(
            validate_binding(
                &record(NamespacePurpose::Observations, &["build"], &[], 1),
                &fleet
            )
            .is_err()
        );
        assert!(
            validate_binding(
                &record(NamespacePurpose::ExecGrants, &["build"], &[], 0),
                &fleet
            )
            .is_err()
        );
    }

    #[test]
    fn issuer_reactivation_names_the_revoked_revision() {
        let revoked = issuer_record(&key(1), &key(6), "fleet", 2, 10, false, None).unwrap();
        assert_eq!(revoked.status, IssuerStatus::Revoked);
        assert!(issuer_record(&key(1), &key(6), "fleet", 3, 10, true, None).is_err());
        assert!(issuer_record(&key(1), &key(6), "fleet", 3, 10, true, Some(3)).is_err());
        assert!(issuer_record(&key(1), &key(1), "fleet", 3, 10, false, None).is_err());
        let active = issuer_record(&key(1), &key(6), "fleet", 3, 10, true, Some(2)).unwrap();
        assert_eq!(active.reauthorizes_revision, Some(2));
    }

    #[test]
    fn revisions_only_increase_or_replay_identically() {
        assert!(check_revision(None, 1, b"a").unwrap());
        assert!(!check_revision(Some((1, b"a".to_vec())), 1, b"a").unwrap());
        assert!(check_revision(Some((1, b"a".to_vec())), 1, b"b").is_err());
        assert!(check_revision(Some((2, b"a".to_vec())), 1, b"b").is_err());
        assert!(check_revision(Some((1, b"a".to_vec())), 2, b"b").unwrap());
    }

    #[test]
    fn observer_grants_open_only_for_their_observer() {
        let root = aster::SecretKey::from_bytes([1; 32]);
        let observer = aster::SecretKey::from_bytes([6; 32]);
        let secret = aster::NamespaceSecret::from_bytes([9; 32]);
        let (key_text, record) = observer_grant(&key(1), &key(6), &secret).unwrap();
        let granter = aster::NodeId::from_hex(hex::encode(key(1)));
        let recipient = aster::NodeId::from_hex(hex::encode(key(6)));
        let resource = secret.id().to_bytes();
        let context = GrantContext {
            app: OBSERVER_GRANT_APP,
            granter: &granter,
            recipient: &recipient,
            resource: &resource,
            path: &key_text,
            role: "write",
        };
        let opened = aster::grants::reconcile_namespace_grant(&observer, &context, Some(&record))
            .active()
            .unwrap();
        assert!(opened.can_write());
        assert_eq!(opened.namespace_id(), secret.id());
        assert!(
            aster::grants::reconcile_namespace_grant(&root, &context, Some(&record))
                .active()
                .is_none()
        );
    }
}
