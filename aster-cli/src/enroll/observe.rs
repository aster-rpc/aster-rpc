//! Observations and promotion (enrollment B10). The root reviews what
//! observers wrote and promotes persistent nodes to root-authored
//! registrations; an issuer writes an `issued` observation when it enrolls a
//! persistent node. Review and promotion semantics live in Aster core.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use aster::enrollment::{
    AdmissionClaims, EnrollmentPolicyStore, JwtEnrollmentVerifier, NamespacePurpose,
    NodeRegistrationV1, ObservationKind, ObservationRejection, PolicyStores, ReviewedObservation,
    issue_observation, node_registration_key, observation_key, promotion_record, refresh_policy,
    review_observations,
};
use aster_cli_kit::out::{Report, Styler};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use clap::{Args, ValueEnum};
use serde::Serialize;

use super::publish::{FleetArgs, Session, check_revision, hex32};

#[derive(Debug, Args)]
pub struct ObservationsArgs {
    /// Observations namespace public ID (64 lowercase hex characters)
    #[arg(long)]
    observations_namespace: String,
    /// Owner-only file containing the observations namespace write secret
    #[arg(long, value_name = "FILE")]
    observations_secret_file: PathBuf,
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    #[command(flatten)]
    observations: ObservationsArgs,
    #[command(flatten)]
    fleet: FleetArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ClaimsSource {
    /// The verified claims of the observed chain's leaf
    Observed,
    /// Only the --scope/--tag values given here
    Explicit,
}

#[derive(Debug, Args)]
pub struct PromoteArgs {
    /// Node to promote; repeatable
    #[arg(long)]
    subject: Vec<String>,
    /// Also promote every accepted node whose chain this issuer signed; repeatable
    #[arg(long, value_name = "KEY")]
    auto_issuers: Vec<String>,
    /// Registration revision for each promoted node
    #[arg(long)]
    revision: u64,
    #[arg(long, value_enum, default_value_t = ClaimsSource::Observed)]
    claims: ClaimsSource,
    /// Scope for --claims explicit; repeatable
    #[arg(long)]
    scope: Vec<String>,
    /// Tag for --claims explicit; repeatable
    #[arg(long)]
    tag: Vec<String>,
    /// Membership namespace to register into (default: the fleet namespace)
    #[arg(long, requires = "namespace_secret_file")]
    namespace: Option<String>,
    /// Owner-only file with the membership namespace write secret
    #[arg(long, value_name = "FILE")]
    namespace_secret_file: Option<PathBuf>,
    #[command(flatten)]
    observations: ObservationsArgs,
    #[command(flatten)]
    fleet: FleetArgs,
}

/// For an issuer: publish an `issued` observation of a persistent node.
#[derive(Debug, Args)]
pub struct IssuerObservationArgs {
    /// Observations namespace public ID; write an `issued` observation there
    #[arg(long, requires_all = ["observations_secret_file", "fleet_namespace", "peer"])]
    pub(super) observations_namespace: Option<String>,
    /// Owner-only file with the observations write secret (from the issuer's sealed grant)
    #[arg(long, value_name = "FILE")]
    observations_secret_file: Option<PathBuf>,
    /// Fleet namespace public ID the policy replicas enroll this session under
    #[arg(long)]
    fleet_namespace: Option<String>,
    /// Replica to deliver the observation to (an `aster1…` address,
    /// `<node id>@<ip:port>`, or a node id); repeatable
    #[arg(long)]
    peer: Vec<String>,
}

#[derive(Serialize)]
struct ReviewedEntry {
    subject: String,
    observer: String,
    kind: ObservationKind,
    chain_issuer: Option<String>,
    claims: AdmissionClaims,
    first_seen: u64,
    last_seen: u64,
}

impl From<&ReviewedObservation> for ReviewedEntry {
    fn from(reviewed: &ReviewedObservation) -> Self {
        Self {
            subject: hex::encode(reviewed.observation.subject),
            observer: hex::encode(reviewed.observation.observer),
            kind: reviewed.observation.kind,
            chain_issuer: reviewed.chain_issuer().map(hex::encode),
            claims: reviewed.evidence.claims().clone(),
            first_seen: reviewed.observation.first_seen,
            last_seen: reviewed.observation.last_seen,
        }
    }
}

#[derive(Serialize)]
struct IgnoredEntry {
    key: String,
    author: String,
    reason: String,
}

#[derive(Serialize)]
struct ReviewReport {
    accepted: Vec<ReviewedEntry>,
    ignored: Vec<IgnoredEntry>,
}

impl Report for ReviewReport {
    fn human(&self, _: Styler) -> String {
        let mut text = format!(
            "{} promotable node(s), {} ignored observation(s)",
            self.accepted.len(),
            self.ignored.len()
        );
        for entry in &self.accepted {
            text.push_str(&format!(
                "\n  {} observed {:?} by {} (chain issuer {}), last seen {}",
                entry.subject,
                entry.kind,
                entry.observer,
                entry.chain_issuer.as_deref().unwrap_or("-"),
                entry.last_seen
            ));
        }
        for entry in &self.ignored {
            text.push_str(&format!("\n  ignored {}: {}", entry.key, entry.reason));
        }
        text
    }
}

#[derive(Serialize)]
struct PromotedReport {
    namespace: String,
    promoted: Vec<String>,
    unchanged: Vec<String>,
    revision: u64,
}

impl Report for PromotedReport {
    fn human(&self, _: Styler) -> String {
        format!(
            "Promoted {} node(s) into {} at revision {}{}\nDelivered to every policy replica.",
            self.promoted.len(),
            self.namespace,
            self.revision,
            if self.unchanged.is_empty() {
                String::new()
            } else {
                format!(" ({} already published)", self.unchanged.len())
            }
        )
    }
}

fn reason(rejection: &ObservationRejection) -> String {
    match rejection {
        ObservationRejection::Invalid(error) => format!("invalid observation: {error:?}"),
        ObservationRejection::Pending => "content not yet synced".into(),
        ObservationRejection::ObserverNotQualified => "observer is not qualified".into(),
        ObservationRejection::ChainInvalid(error) => format!("chain does not verify: {error:?}"),
        ObservationRejection::Ephemeral => "ephemeral credential; never promoted".into(),
        ObservationRejection::AlreadyRegistered => "already registered or denied".into(),
    }
}

/// Sync the fleet and observations namespaces from the replicas and review.
async fn review(
    ctx: &Ctx,
    session: &Session,
    fleet_secret: &std::path::Path,
    observations: &ObservationsArgs,
) -> Result<(
    aster::Doc,
    aster::Doc,
    aster::enrollment::ObservationReview,
    PolicyStores,
)> {
    let fleet = session
        .writable(session.fleet_namespace(), fleet_secret)
        .await?;
    let namespace = hex32(
        &observations.observations_namespace,
        "observations namespace",
    )?;
    let observations = session
        .writable(&namespace, &observations.observations_secret_file)
        .await?;
    let stores = PolicyStores::new(Arc::new(EnrollmentPolicyStore::open(
        Session::state_path(
            ctx,
            &format!("review-{}.json", hex::encode(session.fleet_namespace())),
        )?,
        *session.root(),
        *session.fleet_namespace(),
        session.audience(),
    )?));
    refresh_policy(&stores, &fleet, &[]).await?;
    if !stores.fleet().bindings().iter().any(|binding| {
        binding.purpose == NamespacePurpose::Observations
            && binding.namespace == hex::encode(namespace)
    }) {
        return Err(Error::usage(
            "the fleet does not bind this namespace as its observations namespace",
        ));
    }
    let verifier = JwtEnrollmentVerifier::new(*session.root(), session.audience())?;
    let now = super::unix_now()?;
    let review = review_observations(&observations, &fleet, &verifier, &stores, now).await?;
    Ok((fleet, observations, review, stores))
}

pub async fn run_review(ctx: &Ctx, args: ReviewArgs) -> Result<()> {
    let session = Session::start(ctx, &args.fleet).await?;
    let (fleet, observations, review, _) = review(
        ctx,
        &session,
        &args.fleet.fleet_secret_file,
        &args.observations,
    )
    .await?;
    session.close(vec![fleet, observations]).await?;
    ctx.emit(&ReviewReport {
        accepted: review.accepted.iter().map(ReviewedEntry::from).collect(),
        ignored: review
            .rejected
            .iter()
            .map(|(key, author, rejection)| IgnoredEntry {
                key: String::from_utf8_lossy(key).into_owned(),
                author: author.clone(),
                reason: reason(rejection),
            })
            .collect(),
    })
}

/// Which accepted observations to promote: the named subjects (each must be
/// accepted) plus, with `--auto-issuers`, every node one of those issuers
/// enrolled.
pub(super) fn select<'a>(
    accepted: &'a [ReviewedObservation],
    subjects: &BTreeSet<[u8; 32]>,
    auto_issuers: &BTreeSet<[u8; 32]>,
) -> Result<Vec<&'a ReviewedObservation>> {
    for subject in subjects {
        if !accepted
            .iter()
            .any(|reviewed| reviewed.observation.subject == *subject)
        {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "no accepted observation for {}; run `aster enroll review`",
                    hex::encode(subject)
                ),
            ));
        }
    }
    Ok(accepted
        .iter()
        .filter(|reviewed| {
            subjects.contains(&reviewed.observation.subject)
                || reviewed
                    .chain_issuer()
                    .is_some_and(|issuer| auto_issuers.contains(&issuer))
        })
        .collect())
}

pub async fn run_promote(ctx: &Ctx, args: PromoteArgs) -> Result<()> {
    if args.subject.is_empty() && args.auto_issuers.is_empty() {
        return Err(Error::usage("name a --subject or --auto-issuers"));
    }
    if args.revision == 0 {
        return Err(Error::usage("revision must be positive"));
    }
    let explicit = match args.claims {
        ClaimsSource::Observed if !args.scope.is_empty() || !args.tag.is_empty() => {
            return Err(Error::usage("--scope and --tag need --claims explicit"));
        }
        ClaimsSource::Observed => None,
        ClaimsSource::Explicit => Some(AdmissionClaims {
            scopes: Some(args.scope.iter().cloned().collect()),
            tags: Some(args.tag.iter().cloned().collect()),
            attributes: Some(Default::default()),
        }),
    };
    let subjects = args
        .subject
        .iter()
        .map(|value| hex32(value, "subject"))
        .collect::<Result<BTreeSet<_>>>()?;
    let auto_issuers = args
        .auto_issuers
        .iter()
        .map(|value| hex32(value, "issuer"))
        .collect::<Result<BTreeSet<_>>>()?;
    let session = Session::start(ctx, &args.fleet).await?;
    let (fleet, observations, review, stores) = review(
        ctx,
        &session,
        &args.fleet.fleet_secret_file,
        &args.observations,
    )
    .await?;
    let chosen = select(&review.accepted, &subjects, &auto_issuers)?;
    let (target, namespace) = match (&args.namespace, &args.namespace_secret_file) {
        (Some(namespace), Some(secret)) => {
            let namespace = hex32(namespace, "membership namespace")?;
            if !stores.fleet().bindings().iter().any(|binding| {
                binding.purpose == NamespacePurpose::Membership
                    && binding.namespace == hex::encode(namespace)
            }) {
                return Err(Error::usage(
                    "the fleet does not bind this namespace as a membership namespace",
                ));
            }
            (session.writable(&namespace, secret).await?, namespace)
        }
        _ => (fleet.clone(), *session.fleet_namespace()),
    };
    let now = super::unix_now()?;
    let mut promoted = Vec::new();
    let mut unchanged = Vec::new();
    for reviewed in chosen {
        let record = promotion_record(
            session.root(),
            reviewed,
            session.audience(),
            args.revision,
            explicit.clone(),
            now,
        );
        let key = node_registration_key(&reviewed.observation.subject);
        let previous = session
            .read_root(&target, &key)
            .await?
            .map(|bytes| {
                serde_json::from_slice::<NodeRegistrationV1>(&bytes)
                    .map(|old| (old.revision, bytes))
                    .map_err(|_| Error::usage("the published registration is unreadable"))
            })
            .transpose()?;
        let bytes = serde_json::to_vec(&record)?;
        if check_revision(previous, record.revision, &bytes)? {
            session.put_root(&target, &key, bytes).await?;
            promoted.push(record.subject);
        } else {
            unchanged.push(record.subject);
        }
    }
    observations.leave().await?;
    let mut written = vec![target.clone()];
    if namespace != *session.fleet_namespace() {
        written.push(fleet);
    }
    session.finish(written).await?;
    ctx.emit(&PromotedReport {
        namespace: hex::encode(namespace),
        promoted,
        unchanged,
        revision: args.revision,
    })
}

/// After issuing a persistent credential as an issuer, write the `issued`
/// observation the root reviews. Writes as the issuer's own key.
pub(super) async fn publish_issued(
    ctx: &Ctx,
    args: &IssuerObservationArgs,
    audience: &str,
    issuer_secret: &zeroize::Zeroizing<[u8; 32]>,
    subject: &[u8; 32],
    chain: &str,
    now: u64,
) -> Result<Option<String>> {
    let (Some(namespace), Some(secret_file), Some(fleet)) = (
        &args.observations_namespace,
        &args.observations_secret_file,
        &args.fleet_namespace,
    ) else {
        return Ok(None);
    };
    let namespace = hex32(namespace, "observations namespace")?;
    let token = issue_observation(
        issuer_secret,
        subject,
        chain,
        ObservationKind::Issued,
        now,
        now,
        now,
    )
    .map_err(|e| Error::usage(format!("cannot sign the observation: {e}")))?;
    let observer = aster::enrollment::JwtEnrollmentIssuer::from_seed(issuer_secret).public_key();
    let session = Session::start_as(
        ctx,
        fleet,
        audience,
        &args.peer,
        Some(zeroize::Zeroizing::new(**issuer_secret)),
    )
    .await?;
    let doc = session.writable(&namespace, secret_file).await?;
    let key = observation_key(&observer, subject);
    session.put_root(&doc, &key, token.into_bytes()).await?;
    session.finish(vec![doc]).await?;
    Ok(Some(String::from_utf8_lossy(&key).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster::delegation::{DelegationGrant, LinkKind, Validity, issue_delegation};
    use aster::enrollment::{
        Audience, EnrollmentIssueRequest, Registration, issue_delegated_credential,
    };

    fn key(seed: u8) -> [u8; 32] {
        aster::enrollment::JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
    }

    /// Reviewed observations need real verified chains: review them through
    /// Aster core over a local root node.
    async fn reviewed(subjects: &[(u8, u8)]) -> Vec<ReviewedObservation> {
        let now = 1_800_000_000;
        let node = aster::Node::start(
            aster::AsterConfig::builder()
                .relay(aster::RelayMode::Disabled)
                .secret_key(aster::SecretKey::from_bytes([1; 32]))
                .build(),
        )
        .await
        .unwrap();
        let fleet = node.docs().create().await.unwrap();
        let observations = node.docs().create().await.unwrap();
        let author = node.docs().default_author().await.unwrap();
        for (issuer, subject) in subjects {
            let grant = DelegationGrant {
                may_issue: [LinkKind::Enrollment].into(),
                audiences: ["fleet".to_owned()].into(),
                scopes: Default::default(),
                tags: Default::default(),
                attribute_keys: Default::default(),
                max_child_lifetime: 86_400,
                max_depth: 0,
                subjects: None,
                operations: Default::default(),
                mandatory: Default::default(),
                may_register: true,
            };
            let delegation = issue_delegation(
                &aster::SecretKey::from_bytes([1; 32]),
                None,
                &aster::PublicKey::from_bytes(key(*issuer)),
                &Audience::One("fleet".into()),
                &grant,
                Validity {
                    issued_at: now - 600,
                    not_before: None,
                    expires_at: now + 86_400,
                },
            )
            .unwrap();
            let mut request = EnrollmentIssueRequest::new(key(*subject), "fleet", now + 3600);
            request.registration = Registration::Persistent;
            let chain = issue_delegated_credential(
                &aster::SecretKey::from_bytes([*issuer; 32]),
                &delegation,
                &request,
                now - 60,
            )
            .unwrap()
            .into_chain();
            let token = issue_observation(
                &[*issuer; 32],
                &key(*subject),
                &chain,
                ObservationKind::Issued,
                now - 30,
                now - 30,
                now - 30,
            )
            .unwrap();
            observations
                .set_bytes(
                    &author,
                    observation_key(&key(*issuer), &key(*subject)),
                    token.into_bytes(),
                )
                .await
                .unwrap();
        }
        let temp = std::env::temp_dir().join(format!(
            "aster-cli-observe-{}-{}",
            std::process::id(),
            subjects.len()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let namespace: [u8; 32] = hex::decode(fleet.id().unwrap().to_string())
            .unwrap()
            .try_into()
            .unwrap();
        let stores = PolicyStores::new(Arc::new(
            EnrollmentPolicyStore::open(temp.join("fleet.json"), key(1), namespace, "fleet")
                .unwrap(),
        ));
        let verifier = JwtEnrollmentVerifier::new(key(1), "fleet").unwrap();
        let review = review_observations(&observations, &fleet, &verifier, &stores, now)
            .await
            .unwrap();
        node.shutdown().await;
        let _ = std::fs::remove_dir_all(&temp);
        review.accepted
    }

    #[tokio::test]
    async fn promotion_selects_named_subjects_and_auto_issuers() {
        let accepted = reviewed(&[(6, 20), (6, 21), (9, 22)]).await;
        assert_eq!(accepted.len(), 3);
        let subjects = |list: &[u8]| list.iter().map(|seed| key(*seed)).collect();
        let pick = |named: &[u8], issuers: &[u8]| -> BTreeSet<[u8; 32]> {
            select(&accepted, &subjects(named), &subjects(issuers))
                .unwrap()
                .into_iter()
                .map(|reviewed| reviewed.observation.subject)
                .collect()
        };
        assert_eq!(pick(&[22], &[]), subjects(&[22]));
        assert_eq!(pick(&[], &[6]), subjects(&[20, 21]));
        assert_eq!(pick(&[22], &[6]), subjects(&[20, 21, 22]));
        assert!(pick(&[], &[7]).is_empty());
        assert!(select(&accepted, &subjects(&[23]), &BTreeSet::new()).is_err());
    }
}
