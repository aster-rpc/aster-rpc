//! Revoke and reauthorize registered nodes as the root (enrollment §5).
//!
//! A revocation is written into the subject's membership namespace and
//! mirrored into the fleet namespace, so verifiers that never received the
//! membership namespace still enforce it. A reauthorization publishes the
//! active record, naming the revoked revision, in both places: the fleet
//! copy must outrank the mirrored denial, which can never be deleted.

use std::path::PathBuf;

use aster::enrollment::{
    AdmissionClaims, Audience, MAX_REGISTRATION_REASON_BYTES, NamespacePurpose,
    NodeRegistrationStatus, NodeRegistrationV1, node_registration_key,
};
use aster_cli_kit::out::{Report, Styler};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use clap::Args;
use serde::Serialize;

use super::publish::{FleetArgs, Session, check_revision, hex32};

#[derive(Debug, Args)]
pub struct MembershipArgs {
    /// The subject's membership namespace, when its registration lives there
    #[arg(
        long,
        requires = "namespace_secret_file",
        conflicts_with = "fleet_only"
    )]
    namespace: Option<String>,
    /// Owner-only file with the membership namespace write secret
    #[arg(long, value_name = "FILE", requires = "namespace")]
    namespace_secret_file: Option<PathBuf>,
    /// The subject has no record in any membership namespace; write the fleet only
    #[arg(long)]
    fleet_only: bool,
}

#[derive(Debug, Args)]
pub struct RevokeArgs {
    /// The node to revoke (64 lowercase hex characters)
    #[arg(long)]
    subject: String,
    /// Operator note recorded with the revocation
    #[arg(long)]
    reason: Option<String>,
    /// Revision to publish; default: one above the highest published
    #[arg(long)]
    revision: Option<u64>,
    #[command(flatten)]
    membership: MembershipArgs,
    #[command(flatten)]
    fleet: FleetArgs,
}

#[derive(Debug, Args)]
pub struct ReauthorizeArgs {
    /// The revoked node to re-admit (64 lowercase hex characters)
    #[arg(long)]
    subject: String,
    /// Revision to publish; default: one above the revoked one
    #[arg(long)]
    revision: Option<u64>,
    /// Replace the revoked record's scopes; repeatable (with --tag, explicit claims)
    #[arg(long)]
    scope: Vec<String>,
    /// Replace the revoked record's tags; repeatable
    #[arg(long)]
    tag: Vec<String>,
    #[command(flatten)]
    membership: MembershipArgs,
    #[command(flatten)]
    fleet: FleetArgs,
}

/// A namespace's current root record for the subject, as published.
#[derive(Clone)]
pub(super) struct Published {
    record: NodeRegistrationV1,
    bytes: Vec<u8>,
}

impl Published {
    pub(super) fn parse(bytes: Vec<u8>) -> Result<Self> {
        let record = serde_json::from_slice(&bytes).map_err(|_| {
            Error::usage("the published registration is unreadable; inspect it first")
        })?;
        Ok(Self { record, bytes })
    }
}

/// What to write: one record, and for each target namespace whether it
/// needs writing (an identical record there is a replay).
#[derive(Debug)]
pub(super) struct Plan {
    pub record: NodeRegistrationV1,
    pub bytes: Vec<u8>,
    pub writes: Vec<bool>,
}

pub(super) enum Change {
    Revoke { reason: Option<String> },
    Reauthorize { claims: Option<AdmissionClaims> },
}

/// Plan a revocation or reauthorization of `subject` across `targets` (the
/// membership namespace first if any, then the fleet), given what each one
/// holds now. Rerunning a completed change replays it unchanged, so a
/// failed delivery can be retried.
pub(super) fn plan(
    root: &[u8; 32],
    audience: &str,
    subject: &[u8; 32],
    targets: &[Option<Published>],
    revision: Option<u64>,
    change: Change,
    now: u64,
) -> Result<Plan> {
    let latest = targets
        .iter()
        .flatten()
        .max_by_key(|published| {
            (
                published.record.revision,
                published.record.status != NodeRegistrationStatus::Active,
            )
        })
        .cloned();
    let highest = latest.as_ref().map_or(0, |latest| latest.record.revision);
    let denied = latest
        .as_ref()
        .filter(|latest| latest.record.status != NodeRegistrationStatus::Active);
    let record = match change {
        Change::Revoke { reason } => {
            if reason.as_ref().is_some_and(|reason| {
                reason.len() > MAX_REGISTRATION_REASON_BYTES || reason.chars().any(char::is_control)
            }) {
                return Err(Error::usage(format!(
                    "reason must be at most {MAX_REGISTRATION_REASON_BYTES} bytes without control characters"
                )));
            }
            // A completed revocation is replayed, not re-revisioned.
            if let (Some(denied), None) = (denied, revision)
                && denied.record.status == NodeRegistrationStatus::Revoked
            {
                return finish(denied.record.clone(), targets);
            }
            let template = latest.as_ref().map(|latest| &latest.record);
            NodeRegistrationV1 {
                version: 1,
                issuer: hex::encode(root),
                subject: hex::encode(subject),
                audience: template.map_or_else(
                    || Audience::One(audience.to_owned()),
                    |old| old.audience.clone(),
                ),
                revision: revision.unwrap_or(highest + 1),
                issued_at: now,
                not_before: None,
                expires_at: None,
                status: NodeRegistrationStatus::Revoked,
                claims: template.map(|old| old.claims.clone()).unwrap_or_default(),
                mandatory: vec![],
                reauthorizes_revision: None,
                reason,
            }
        }
        Change::Reauthorize { claims } => {
            // A completed reauthorization is replayed, not re-revisioned.
            if let (Some(active), None) = (
                latest
                    .as_ref()
                    .filter(|latest| latest.record.status == NodeRegistrationStatus::Active),
                revision,
            ) && active.record.reauthorizes_revision.is_some()
            {
                return finish(active.record.clone(), targets);
            }
            let Some(denied) = denied else {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "{} has no published revocation to reauthorize",
                        hex::encode(subject)
                    ),
                ));
            };
            NodeRegistrationV1 {
                version: 1,
                issuer: hex::encode(root),
                subject: hex::encode(subject),
                audience: denied.record.audience.clone(),
                revision: revision.unwrap_or(denied.record.revision + 1),
                issued_at: now,
                not_before: None,
                expires_at: None,
                status: NodeRegistrationStatus::Active,
                claims: claims.unwrap_or_else(|| denied.record.claims.clone()),
                mandatory: vec![],
                reauthorizes_revision: Some(denied.record.revision),
                reason: None,
            }
        }
    };
    if record.revision <= highest {
        return Err(Error::usage(format!(
            "revision must exceed the highest published revision {highest}"
        )));
    }
    finish(record, targets)
}

fn finish(record: NodeRegistrationV1, targets: &[Option<Published>]) -> Result<Plan> {
    let bytes = serde_json::to_vec(&record)?;
    let writes = targets
        .iter()
        .map(|current| {
            check_revision(
                current
                    .as_ref()
                    .map(|current| (current.record.revision, current.bytes.clone())),
                record.revision,
                &bytes,
            )
        })
        .collect::<Result<_>>()?;
    Ok(Plan {
        record,
        bytes,
        writes,
    })
}

#[derive(Serialize)]
struct ChangedReport {
    action: &'static str,
    subject: String,
    revision: u64,
    status: NodeRegistrationStatus,
    namespaces: Vec<String>,
    written: Vec<String>,
}

impl Report for ChangedReport {
    fn human(&self, _: Styler) -> String {
        format!(
            "{} {} at revision {} in {}{}\nDelivered to every policy replica.",
            self.action,
            self.subject,
            self.revision,
            self.namespaces.join(", "),
            if self.written.is_empty() {
                " (already published)".to_owned()
            } else {
                String::new()
            }
        )
    }
}

pub async fn revoke(ctx: &Ctx, args: RevokeArgs) -> Result<()> {
    let subject = hex32(&args.subject, "subject")?;
    publish(
        ctx,
        &args.fleet,
        &args.membership,
        &subject,
        args.revision,
        Change::Revoke {
            reason: args.reason,
        },
    )
    .await
}

pub async fn reauthorize(ctx: &Ctx, args: ReauthorizeArgs) -> Result<()> {
    let subject = hex32(&args.subject, "subject")?;
    let claims = (!args.scope.is_empty() || !args.tag.is_empty()).then(|| AdmissionClaims {
        scopes: Some(args.scope.into_iter().collect()),
        tags: Some(args.tag.into_iter().collect()),
        attributes: Some(Default::default()),
    });
    publish(
        ctx,
        &args.fleet,
        &args.membership,
        &subject,
        args.revision,
        Change::Reauthorize { claims },
    )
    .await
}

async fn publish(
    ctx: &Ctx,
    fleet_args: &FleetArgs,
    membership: &MembershipArgs,
    subject: &[u8; 32],
    revision: Option<u64>,
    change: Change,
) -> Result<()> {
    let action = match change {
        Change::Revoke { .. } => "Revoked",
        Change::Reauthorize { .. } => "Reauthorized",
    };
    let session = Session::start(ctx, fleet_args).await?;
    let fleet = session
        .writable(session.fleet_namespace(), &fleet_args.fleet_secret_file)
        .await?;
    let bound = membership_bindings(ctx, &session, &fleet).await?;
    let mut docs = Vec::new();
    let mut namespaces = Vec::new();
    match (&membership.namespace, &membership.namespace_secret_file) {
        (Some(namespace), Some(secret)) => {
            let namespace = hex32(namespace, "membership namespace")?;
            if !bound.contains(&hex::encode(namespace)) {
                return Err(Error::usage(
                    "the fleet does not bind this namespace as a membership namespace",
                ));
            }
            docs.push(session.writable(&namespace, secret).await?);
            namespaces.push(hex::encode(namespace));
        }
        _ if !bound.is_empty() && !membership.fleet_only => {
            // A denial below a membership record's revision would lose to it.
            return Err(Error::usage(
                "the fleet binds membership namespaces: name the subject's with --namespace and --namespace-secret-file, or pass --fleet-only if it has none",
            ));
        }
        _ => {}
    }
    docs.push(fleet);
    namespaces.push(hex::encode(session.fleet_namespace()));
    let key = node_registration_key(subject);
    let mut targets = Vec::new();
    for doc in &docs {
        targets.push(
            session
                .read_root(doc, &key)
                .await?
                .map(Published::parse)
                .transpose()?,
        );
    }
    let plan = plan(
        session.root(),
        session.audience(),
        subject,
        &targets,
        revision,
        change,
        super::unix_now()?,
    )?;
    let mut written = Vec::new();
    for ((doc, write), namespace) in docs.iter().zip(&plan.writes).zip(&namespaces) {
        if *write {
            session.put_root(doc, &key, plan.bytes.clone()).await?;
            written.push(namespace.clone());
        }
    }
    session.finish(docs).await?;
    ctx.emit(&ChangedReport {
        action,
        subject: hex::encode(subject),
        revision: plan.record.revision,
        status: plan.record.status,
        namespaces,
        written,
    })
}

/// The membership namespaces the fleet currently binds.
async fn membership_bindings(
    ctx: &Ctx,
    session: &Session,
    fleet: &aster::Doc,
) -> Result<Vec<String>> {
    use aster::enrollment::{EnrollmentPolicyStore, PolicyStores, refresh_policy};
    let stores = PolicyStores::new(std::sync::Arc::new(EnrollmentPolicyStore::open(
        Session::state_path(
            ctx,
            &format!("bindings-{}.json", hex::encode(session.fleet_namespace())),
        )?,
        *session.root(),
        *session.fleet_namespace(),
        session.audience(),
    )?));
    refresh_policy(&stores, fleet, &[]).await?;
    Ok(stores
        .fleet()
        .bindings()
        .into_iter()
        .filter(|binding| binding.purpose == NamespacePurpose::Membership)
        .map(|binding| binding.namespace)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster::enrollment::{
        EnrollmentPolicyStore, JwtEnrollmentIssuer, NamespaceBindingV1, PolicyStores, RecordError,
        namespace_binding_key, refresh_policy,
    };
    use std::sync::Arc;

    const FLEET: &str = "registration-test";

    fn key(seed: u8) -> [u8; 32] {
        JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
    }

    fn active(revision: u64) -> Published {
        let record = NodeRegistrationV1 {
            version: 1,
            issuer: hex::encode(key(1)),
            subject: hex::encode(key(7)),
            audience: Audience::One(FLEET.into()),
            revision,
            issued_at: 1,
            not_before: None,
            expires_at: None,
            status: NodeRegistrationStatus::Active,
            claims: AdmissionClaims {
                scopes: Some(["build".to_owned()].into()),
                tags: Some(Default::default()),
                attributes: Some(Default::default()),
            },
            mandatory: vec![],
            reauthorizes_revision: None,
            reason: None,
        };
        Published {
            bytes: serde_json::to_vec(&record).unwrap(),
            record,
        }
    }

    fn revoke(targets: &[Option<Published>], revision: Option<u64>) -> Result<Plan> {
        plan(
            &key(1),
            FLEET,
            &key(7),
            targets,
            revision,
            Change::Revoke {
                reason: Some("compromised".into()),
            },
            50,
        )
    }

    fn reauthorize(targets: &[Option<Published>]) -> Result<Plan> {
        plan(
            &key(1),
            FLEET,
            &key(7),
            targets,
            None,
            Change::Reauthorize { claims: None },
            60,
        )
    }

    fn published(plan: &Plan) -> Published {
        Published::parse(plan.bytes.clone()).unwrap()
    }

    #[test]
    fn revocation_outranks_the_membership_record_and_replays() {
        // Membership holds revision 4; the fleet holds nothing yet.
        let plan = revoke(&[Some(active(4)), None], None).unwrap();
        assert_eq!(plan.record.revision, 5);
        assert_eq!(plan.record.status, NodeRegistrationStatus::Revoked);
        assert_eq!(plan.record.claims, active(4).record.claims);
        assert_eq!(plan.record.reason.as_deref(), Some("compromised"));
        assert_eq!(plan.writes, vec![true, true]);
        // A rerun after a partial delivery replays the same record.
        let partial = [Some(published(&plan)), None];
        let replay = revoke(&partial, None).unwrap();
        assert_eq!(replay.bytes, plan.bytes);
        assert_eq!(replay.writes, vec![false, true]);
        // An explicit revision must still outrank what is published.
        assert!(revoke(&[Some(active(4)), None], Some(4)).is_err());
        assert!(
            super::plan(
                &key(1),
                FLEET,
                &key(7),
                &[None],
                None,
                Change::Revoke {
                    reason: Some("x".repeat(MAX_REGISTRATION_REASON_BYTES + 1)),
                },
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn reauthorization_names_the_revoked_revision_everywhere() {
        assert!(reauthorize(&[Some(active(4)), None]).is_err());
        let revoked = published(&revoke(&[Some(active(4)), None], None).unwrap());
        let plan = reauthorize(&[Some(revoked.clone()), Some(revoked.clone())]).unwrap();
        assert_eq!(plan.record.revision, 6);
        assert_eq!(plan.record.status, NodeRegistrationStatus::Active);
        assert_eq!(plan.record.reauthorizes_revision, Some(5));
        assert_eq!(plan.record.claims, active(4).record.claims);
        assert_eq!(plan.writes, vec![true, true]);
        let done = published(&plan);
        let replay = reauthorize(&[Some(done.clone()), Some(revoked)]).unwrap();
        assert_eq!(replay.bytes, plan.bytes);
        assert_eq!(replay.writes, vec![false, true]);
    }

    async fn check(
        verifier: &(PolicyStores, bool),
        fleet: &aster::Doc,
        membership: &aster::Doc,
        subject: &[u8; 32],
    ) -> std::result::Result<aster::enrollment::VerifiedAdmissionEvidence, RecordError> {
        let docs = if verifier.1 {
            vec![membership.clone()]
        } else {
            vec![]
        };
        refresh_policy(&verifier.0, fleet, &docs).await.unwrap();
        verifier.0.select_evidence(subject, None, 100)
    }

    /// INV-E10 end to end through this module's records: a verifier holding
    /// only the fleet namespace enforces the revocation, and the
    /// reauthorization re-admits the node on every verifier.
    #[tokio::test]
    async fn fleet_mirror_is_enforced_without_the_membership_namespace() {
        let files =
            std::env::temp_dir().join(format!("aster-cli-registration-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&files);
        std::fs::create_dir_all(&files).unwrap();
        // The node's default Docs author is its own key; make it the root.
        let root_key = key(1);
        let node = aster::Node::start(
            aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .bind_addr("127.0.0.1:0")
                .secret_key(aster::SecretKey::from_bytes([1; 32]))
                .build(),
        )
        .await
        .unwrap();
        let author = node.docs().default_author().await.unwrap();
        assert_eq!(author.to_string(), hex::encode(root_key));
        let fleet = node.docs().create().await.unwrap();
        let membership = node.docs().create().await.unwrap();
        let id = |doc: &aster::Doc| -> [u8; 32] {
            hex::decode(doc.id().unwrap().to_string())
                .unwrap()
                .try_into()
                .unwrap()
        };
        let binding = NamespaceBindingV1 {
            version: 1,
            issuer: hex::encode(root_key),
            namespace: hex::encode(id(&membership)),
            purpose: NamespacePurpose::Membership,
            read_scopes: ["build".to_owned()].into(),
            write_scopes: Default::default(),
            revision: 1,
            status: aster::enrollment::BindingStatus::Active,
        };
        fleet
            .set_bytes(
                &author,
                namespace_binding_key(&id(&membership)),
                serde_json::to_vec(&binding).unwrap(),
            )
            .await
            .unwrap();
        let subject = key(7);
        let registration_key = node_registration_key(&subject);
        let read = |doc: aster::Doc| {
            let author = author.clone();
            let key = registration_key.clone();
            async move {
                doc.get_exact(&author, key)
                    .await
                    .unwrap()
                    .map(Published::parse)
                    .transpose()
                    .unwrap()
            }
        };
        let apply = |plan: Plan, docs: Vec<aster::Doc>| {
            let author = author.clone();
            let key = registration_key.clone();
            async move {
                for (doc, write) in docs.iter().zip(plan.writes) {
                    if write {
                        doc.set_bytes(&author, key.clone(), plan.bytes.clone())
                            .await
                            .unwrap();
                    }
                }
            }
        };
        // Promoted into the membership namespace at revision 1.
        membership
            .set_bytes(&author, registration_key.clone(), active(1).bytes)
            .await
            .unwrap();

        // Two verifiers: one holds the membership namespace, one only the fleet.
        let verifier = |name: &str, with_membership: bool| {
            let stores = PolicyStores::new(Arc::new(
                EnrollmentPolicyStore::open(files.join(name), root_key, id(&fleet), FLEET).unwrap(),
            ));
            (stores, with_membership)
        };
        let full = verifier("full.json", true);
        let fleet_only = verifier("fleet-only.json", false);
        assert!(check(&full, &fleet, &membership, &subject).await.is_ok());

        let targets = vec![read(membership.clone()).await, read(fleet.clone()).await];
        let plan = revoke(&targets, None).unwrap();
        apply(plan, vec![membership.clone(), fleet.clone()]).await;
        for verifier in [&full, &fleet_only] {
            assert!(matches!(
                check(verifier, &fleet, &membership, &subject).await,
                Err(RecordError::Denied(
                    aster::enrollment::DenialReason::NodeRevoked
                ))
            ));
        }

        let targets = vec![read(membership.clone()).await, read(fleet.clone()).await];
        let plan = reauthorize(&targets).unwrap();
        apply(plan, vec![membership.clone(), fleet.clone()]).await;
        for verifier in [&full, &fleet_only] {
            let evidence = check(verifier, &fleet, &membership, &subject)
                .await
                .unwrap();
            assert!(matches!(
                evidence.source(),
                aster::enrollment::AdmissionEvidenceSource::RootAuthoredRecord { revision: 3, .. }
            ));
        }
        node.shutdown().await;
        let _ = std::fs::remove_dir_all(&files);
    }
}
