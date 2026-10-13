//! Recovery across a root rotation (ownership-attestations §7, decision D3).
//!
//! Nothing the old root published confers authority after a rotation, its
//! denials included: a compromised root could have planted them. The
//! operator exports the old root's active set while it is still readable
//! (`aster enroll export`), reviews and edits the file, and republishes the
//! reviewed set under the new root into fresh namespaces (`aster enroll
//! republish`). Every republished record starts a new history at revision 1.
//! Nodes keep their keys; issuers need new delegation links from the new
//! root (`aster enroll delegate`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use aster::enrollment::{
    BindingStatus, CredentialStatusV1, EnrollmentPolicyStore, IssuerStatusV1, NamespaceBindingV1,
    NamespacePurpose, NodeRegistrationStatus, NodeRegistrationV1, PolicyStores,
    credential_status_key, issuer_status_key, namespace_binding_key, node_registration_key,
    refresh_policy,
};
use aster_cli_kit::out::{Report, Styler};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use clap::Args;
use serde::{Deserialize, Serialize};

use super::publish::{FleetArgs, Session, check_revision, hex32, namespace_secret};
use crate::files::read_bounded;

const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Args)]
pub struct ExportArgs {
    #[command(flatten)]
    fleet: FleetArgs,
    /// Owner-only write secret of a bound membership namespace to include; repeatable
    #[arg(long, value_name = "FILE")]
    membership_secret_file: Vec<PathBuf>,
    /// Export file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
}

#[derive(Debug, Args)]
pub struct RepublishArgs {
    /// The reviewed export of the previous root's records
    #[arg(long, value_name = "FILE")]
    from: PathBuf,
    /// Carry an exported membership namespace over to a new one, as
    /// OLD_NAMESPACE=NEW_SECRET_FILE; repeatable. Registrations from an
    /// unmapped namespace go to the new fleet namespace.
    #[arg(long, value_name = "OLD=FILE")]
    membership: Vec<String>,
    /// The new root's fleet namespace, write secret, audience and replicas
    #[command(flatten)]
    fleet: FleetArgs,
}

/// The old root's records, for review between export and republish.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryExportV1 {
    pub version: u32,
    pub root: String,
    pub audience: String,
    pub fleet_namespace: String,
    pub exported_at: u64,
    /// Registrations, including denials, by namespace (lowercase hex).
    pub registrations: BTreeMap<String, Vec<NodeRegistrationV1>>,
    pub issuers: Vec<IssuerStatusV1>,
    pub credentials: Vec<CredentialStatusV1>,
    pub bindings: Vec<NamespaceBindingV1>,
}

#[derive(Serialize)]
struct ExportedReport {
    file: PathBuf,
    root: String,
    namespaces: usize,
    registrations: usize,
    issuers: usize,
    credentials: usize,
    bindings: usize,
}

impl Report for ExportedReport {
    fn human(&self, _: Styler) -> String {
        format!(
            "Exported the records of root {} to {}\n  {} registrations in {} namespaces, {} issuer statuses, {} credential statuses, {} bindings\nReview and edit the file, rotate (`aster trust rotate --issue`), then `aster enroll republish --from {}`.",
            self.root,
            self.file.display(),
            self.registrations,
            self.namespaces,
            self.issuers,
            self.credentials,
            self.bindings,
            self.file.display(),
        )
    }
}

pub async fn export(ctx: &Ctx, args: ExportArgs) -> Result<()> {
    let session = Session::start(ctx, &args.fleet).await?;
    let fleet = session
        .writable(session.fleet_namespace(), &args.fleet.fleet_secret_file)
        .await?;
    let mut membership = Vec::new();
    for file in &args.membership_secret_file {
        let namespace = namespace_of(file)?;
        membership.push((namespace, session.writable(&namespace, file).await?));
    }
    let stores = PolicyStores::new(Arc::new(EnrollmentPolicyStore::open(
        Session::state_path(
            ctx,
            &format!("export-{}.json", hex::encode(session.fleet_namespace())),
        )?,
        *session.root(),
        *session.fleet_namespace(),
        session.audience(),
    )?));
    let docs: Vec<aster::Doc> = membership.iter().map(|(_, doc)| doc.clone()).collect();
    if refresh_policy(&stores, &fleet, &docs).await? {
        return Err(Error::new(
            ErrorKind::Unavailable,
            "some records are pending or unreadable; the export would be incomplete",
        )
        .with_hint("rerun once every replica holds the records' content"));
    }
    let fleet_records = stores.fleet().accepted_records();
    let mut registrations = BTreeMap::new();
    registrations.insert(
        hex::encode(session.fleet_namespace()),
        fleet_records.registrations,
    );
    for (namespace, _) in &membership {
        let store = stores.membership(namespace).ok_or_else(|| {
            Error::usage(format!(
                "namespace {} is not bound to the fleet as a membership namespace",
                hex::encode(namespace)
            ))
        })?;
        registrations.insert(
            hex::encode(namespace),
            store.accepted_records().registrations,
        );
    }
    let export = RecoveryExportV1 {
        version: 1,
        root: hex::encode(session.root()),
        audience: session.audience().to_owned(),
        fleet_namespace: hex::encode(session.fleet_namespace()),
        exported_at: super::unix_now()?,
        registrations,
        issuers: fleet_records.issuers,
        credentials: fleet_records.credentials,
        bindings: fleet_records.bindings,
    };
    let mut docs = vec![fleet];
    docs.extend(membership.into_iter().map(|(_, doc)| doc));
    session.close(docs).await?;
    aster_keystore::write_secret_file(
        &args.out,
        &format!("{}\n", serde_json::to_string_pretty(&export)?),
    )
    .map_err(|e| Error::from(e).with_kind(ErrorKind::Usage))?;
    ctx.emit(&ExportedReport {
        file: args.out,
        root: export.root,
        namespaces: export.registrations.len(),
        registrations: export.registrations.values().map(Vec::len).sum(),
        issuers: export.issuers.len(),
        credentials: export.credentials.len(),
        bindings: export.bindings.len(),
    })
}

/// One record to write under the new root.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Write {
    pub namespace: [u8; 32],
    pub key: Vec<u8>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct Republished {
    pub active: usize,
    pub denied: usize,
    pub issuers: usize,
    pub credentials: usize,
    pub bindings: usize,
    /// Bindings not carried over: other purposes, and membership namespaces
    /// with no mapping (their registrations went to the fleet namespace).
    pub not_carried: Vec<String>,
    #[serde(skip)]
    pub writes: Vec<Write>,
}

/// Rewrite the reviewed export under `root`: the winning statement for each
/// subject across the exported namespaces (highest revision, a denial
/// winning a tie, an unreauthorized activation losing to the denial it
/// outranks) becomes revision 1 in its new namespace, denials mirrored into
/// the fleet; mapped membership namespaces are bound afresh; issuer
/// statuses and credential revocations move to the fleet namespace.
pub(super) fn plan_republish(
    export: &RecoveryExportV1,
    root: &[u8; 32],
    fleet: [u8; 32],
    mapping: &BTreeMap<String, [u8; 32]>,
    now: u64,
) -> Result<Republished> {
    if export.version != 1 {
        return Err(Error::usage("unsupported export version"));
    }
    if export.root == hex::encode(root) {
        return Err(Error::usage(
            "the export was taken under this profile's root; rotate first (`aster trust rotate --issue`)",
        ));
    }
    let foreign = |issuer: &str| issuer != export.root;
    if export
        .registrations
        .values()
        .flatten()
        .any(|record| foreign(&record.issuer))
        || export.issuers.iter().any(|record| foreign(&record.issuer))
        || export
            .credentials
            .iter()
            .any(|record| foreign(&record.issuer))
        || export.bindings.iter().any(|record| foreign(&record.issuer))
    {
        return Err(Error::usage(
            "the export holds records from another root; it must be one root's export",
        ));
    }
    let mut plan = Republished::default();

    // Bindings: every mapping must name an active membership binding.
    let active_bindings: Vec<&NamespaceBindingV1> = export
        .bindings
        .iter()
        .filter(|binding| binding.status == BindingStatus::Active)
        .collect();
    let mut bound = 0;
    for (old, new) in mapping {
        let binding = active_bindings
            .iter()
            .find(|binding| {
                binding.namespace == *old && binding.purpose == NamespacePurpose::Membership
            })
            .ok_or_else(|| {
                Error::usage(format!(
                    "{old} is not an active membership binding in the export"
                ))
            })?;
        write(
            &mut plan,
            fleet,
            namespace_binding_key(new),
            &NamespaceBindingV1 {
                version: 1,
                issuer: hex::encode(root),
                namespace: hex::encode(new),
                purpose: NamespacePurpose::Membership,
                read_scopes: binding.read_scopes.clone(),
                write_scopes: binding.write_scopes.clone(),
                revision: 1,
                status: BindingStatus::Active,
            },
        )?;
        bound += 1;
    }
    let not_carried = active_bindings
        .iter()
        .filter(|binding| !mapping.contains_key(&binding.namespace))
        .map(|binding| format!("{} ({:?})", binding.namespace, binding.purpose))
        .collect();

    // Registrations: one winner per subject across namespaces.
    let mut subjects: BTreeMap<&str, Vec<(&str, &NodeRegistrationV1)>> = BTreeMap::new();
    for (namespace, records) in &export.registrations {
        for record in records {
            subjects
                .entry(record.subject.as_str())
                .or_default()
                .push((namespace.as_str(), record));
        }
    }
    let (mut active, mut denied) = (0, 0);
    for (subject, statements) in subjects {
        let key = node_registration_key(&hex32(subject, "exported subject")?);
        let is_denial =
            |record: &NodeRegistrationV1| record.status != NodeRegistrationStatus::Active;
        let (namespace, winner) = statements
            .iter()
            .max_by_key(|(_, record)| (record.revision, is_denial(record)))
            .copied()
            .expect("a subject has at least one statement");
        let latest_denial = statements
            .iter()
            .filter(|(_, record)| is_denial(record))
            .map(|(_, record)| *record)
            .max_by_key(|record| record.revision);
        let mut record = winner.clone();
        if let (false, Some(denial)) = (is_denial(winner), latest_denial)
            && winner.reauthorizes_revision != Some(denial.revision)
        {
            record = denial.clone();
        }
        let targets: BTreeSet<[u8; 32]> = if is_denial(&record) {
            denied += 1;
            statements
                .iter()
                .filter_map(|(namespace, _)| mapping.get(*namespace).copied())
                .chain([fleet])
                .collect()
        } else {
            active += 1;
            [mapping.get(namespace).copied().unwrap_or(fleet)].into()
        };
        record.issuer = hex::encode(root);
        record.revision = 1;
        record.issued_at = now;
        record.reauthorizes_revision = None;
        for target in targets {
            write(&mut plan, target, key.clone(), &record)?;
        }
    }

    for status in &export.issuers {
        let mut status = status.clone();
        status.issuer = hex::encode(root);
        status.revision = 1;
        status.issued_at = now;
        status.reauthorizes_revision = None;
        write(
            &mut plan,
            fleet,
            issuer_status_key(&hex32(&status.subject, "exported issuer")?),
            &status,
        )?;
        plan.issuers += 1;
    }
    for status in export.credentials.iter().filter(|status| status.revoked) {
        let mut status = status.clone();
        status.issuer = hex::encode(root);
        status.revision = 1;
        status.issued_at = now;
        status.reauthorizes_revision = None;
        write(
            &mut plan,
            fleet,
            credential_status_key(&status.credential_id),
            &status,
        )?;
        plan.credentials += 1;
    }
    plan.active = active;
    plan.denied = denied;
    plan.bindings = bound;
    plan.not_carried = not_carried;
    Ok(plan)
}

fn write(
    plan: &mut Republished,
    namespace: [u8; 32],
    key: Vec<u8>,
    value: &impl Serialize,
) -> Result<()> {
    plan.writes.push(Write {
        namespace,
        key,
        bytes: serde_json::to_vec(value)?,
    });
    Ok(())
}

#[derive(Serialize)]
struct RepublishedReport {
    root: String,
    from_root: String,
    #[serde(flatten)]
    plan: Republished,
    written: usize,
}

impl Report for RepublishedReport {
    fn human(&self, _: Styler) -> String {
        let mut out = format!(
            "Republished the reviewed records of root {} under root {}\n  {} active and {} denied registrations, {} issuer statuses, {} credential revocations, {} bindings ({} records written)\nDelivered to every policy replica.\nIssuers need new delegations from this root (`aster enroll delegate`).",
            self.from_root,
            self.root,
            self.plan.active,
            self.plan.denied,
            self.plan.issuers,
            self.plan.credentials,
            self.plan.bindings,
            self.written,
        );
        for binding in &self.plan.not_carried {
            out.push_str(&format!("\nNot carried over: binding {binding}"));
        }
        out
    }
}

pub async fn republish(ctx: &Ctx, args: RepublishArgs) -> Result<()> {
    let export: RecoveryExportV1 =
        serde_json::from_slice(&read_bounded(&args.from, MAX_EXPORT_BYTES, false)?)
            .map_err(|e| Error::usage(format!("the export file is not a recovery export: {e}")))?;
    let mut mapping = BTreeMap::new();
    let mut secrets = BTreeMap::new();
    for entry in &args.membership {
        let (old, file) = entry
            .split_once('=')
            .ok_or_else(|| Error::usage("--membership takes OLD_NAMESPACE=NEW_SECRET_FILE"))?;
        hex32(old, "exported membership namespace")?;
        let file = PathBuf::from(file);
        let new = namespace_of(&file)?;
        mapping.insert(old.to_owned(), new);
        secrets.insert(new, file);
    }
    let session = Session::start(ctx, &args.fleet).await?;
    if export.audience != session.audience() {
        return Err(Error::usage(format!(
            "the export is for audience `{}`, not `{}`",
            export.audience,
            session.audience()
        )));
    }
    let plan = plan_republish(
        &export,
        session.root(),
        *session.fleet_namespace(),
        &mapping,
        super::unix_now()?,
    )?;
    let mut docs = BTreeMap::new();
    docs.insert(
        *session.fleet_namespace(),
        session
            .writable(session.fleet_namespace(), &args.fleet.fleet_secret_file)
            .await?,
    );
    for (namespace, file) in &secrets {
        docs.insert(*namespace, session.writable(namespace, file).await?);
    }
    let mut written = 0;
    for write in &plan.writes {
        let doc = &docs[&write.namespace];
        let previous = session
            .read_root(doc, &write.key)
            .await?
            .map(|bytes| (1, bytes));
        // A rerun replays identical records; anything else at revision 1
        // was published by someone else and is not overwritten.
        if check_revision(previous, 1, &write.bytes)? {
            session
                .put_root(doc, &write.key, write.bytes.clone())
                .await?;
            written += 1;
        }
    }
    let root = hex::encode(session.root());
    session.finish(docs.into_values().collect()).await?;
    ctx.emit(&RepublishedReport {
        root,
        from_root: export.root.clone(),
        plan,
        written,
    })
}

/// The namespace a write-secret file opens.
fn namespace_of(file: &std::path::Path) -> Result<[u8; 32]> {
    let bytes = zeroize::Zeroizing::new(read_bounded(file, 128, true)?);
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::usage("namespace secret must contain hex"))?
        .trim();
    let namespace = aster::NamespaceSecret::from_bytes(hex32(text, "namespace secret")?)
        .id()
        .to_bytes();
    namespace_secret(file, &namespace)?;
    Ok(namespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aster::enrollment::{AdmissionClaims, Audience, IssuerStatus, JwtEnrollmentIssuer};

    const FLEET: &str = "recovery-test";

    fn key(seed: u8) -> [u8; 32] {
        JwtEnrollmentIssuer::from_seed(&[seed; 32]).public_key()
    }
    fn registration(
        subject: u8,
        revision: u64,
        status: NodeRegistrationStatus,
        reauthorizes: Option<u64>,
    ) -> NodeRegistrationV1 {
        NodeRegistrationV1 {
            version: 1,
            issuer: hex::encode(key(1)),
            subject: hex::encode(key(subject)),
            audience: Audience::One(FLEET.into()),
            revision,
            issued_at: 5,
            not_before: None,
            expires_at: None,
            status,
            claims: AdmissionClaims::default(),
            mandatory: vec![],
            reauthorizes_revision: reauthorizes,
            reason: None,
        }
    }
    fn binding(namespace: [u8; 32], purpose: NamespacePurpose) -> NamespaceBindingV1 {
        NamespaceBindingV1 {
            version: 1,
            issuer: hex::encode(key(1)),
            namespace: hex::encode(namespace),
            purpose,
            read_scopes: ["build".to_owned()].into(),
            write_scopes: Default::default(),
            revision: 3,
            status: BindingStatus::Active,
        }
    }
    fn export() -> RecoveryExportV1 {
        use NodeRegistrationStatus::*;
        let fleet = hex::encode([10; 32]);
        let build = hex::encode([11; 32]);
        RecoveryExportV1 {
            version: 1,
            root: hex::encode(key(1)),
            audience: FLEET.into(),
            fleet_namespace: fleet.clone(),
            exported_at: 5,
            registrations: [
                (
                    fleet,
                    vec![
                        // Node 7's denial mirrored into the fleet at revision 2.
                        registration(7, 2, Revoked, None),
                        // Node 9: active in the fleet only.
                        registration(9, 4, Active, None),
                    ],
                ),
                (
                    build,
                    vec![
                        // Outranks the denial without naming it: still denied.
                        registration(7, 3, Active, None),
                        // Node 8: active in the membership namespace.
                        registration(8, 6, Active, None),
                    ],
                ),
            ]
            .into(),
            issuers: vec![IssuerStatusV1 {
                version: 1,
                issuer: hex::encode(key(1)),
                subject: hex::encode(key(6)),
                audience: Audience::One(FLEET.into()),
                revision: 2,
                issued_at: 5,
                status: IssuerStatus::Revoked,
                reauthorizes_revision: Some(1),
            }],
            credentials: vec![
                CredentialStatusV1 {
                    version: 1,
                    issuer: hex::encode(key(1)),
                    credential_id: "revoked-credential-1".into(),
                    revision: 4,
                    issued_at: 5,
                    revoked: true,
                    reauthorizes_revision: None,
                },
                CredentialStatusV1 {
                    version: 1,
                    issuer: hex::encode(key(1)),
                    credential_id: "restored-credential".into(),
                    revision: 5,
                    issued_at: 5,
                    revoked: false,
                    reauthorizes_revision: Some(4),
                },
            ],
            bindings: vec![
                binding([11; 32], NamespacePurpose::Membership),
                binding([12; 32], NamespacePurpose::ExecGrants),
            ],
        }
    }
    fn parse<T: serde::de::DeserializeOwned>(write: &Write) -> T {
        serde_json::from_slice(&write.bytes).unwrap()
    }

    /// Republished records restart at revision 1 under the new root; a
    /// subject's winning statement keeps its outcome, denials reach the
    /// fleet namespace, and only mapped membership namespaces are bound.
    #[test]
    fn republish_preserves_outcomes_under_the_new_root() {
        let (fleet, new_build) = ([20; 32], [21; 32]);
        let mapping = [(hex::encode([11; 32]), new_build)].into();
        let plan = plan_republish(&export(), &key(2), fleet, &mapping, 100).unwrap();
        assert_eq!((plan.active, plan.denied), (2, 1));
        assert_eq!((plan.issuers, plan.credentials, plan.bindings), (1, 1, 1));
        assert_eq!(plan.not_carried.len(), 1);
        for write in &plan.writes {
            let value: serde_json::Value = serde_json::from_slice(&write.bytes).unwrap();
            assert_eq!(value["issuer"], hex::encode(key(2)));
            assert_eq!(value["revision"], 1);
        }
        let registrations = |subject: u8| -> Vec<(&[u8; 32], NodeRegistrationV1)> {
            plan.writes
                .iter()
                .filter(|write| write.key == node_registration_key(&key(subject)))
                .map(|write| (&write.namespace, parse(write)))
                .collect()
        };
        // Node 7 stays denied, in the fleet and its mapped namespace.
        let seven = registrations(7);
        assert_eq!(seven.len(), 2);
        assert!(seven.iter().all(|(_, record)| {
            record.status == NodeRegistrationStatus::Revoked
                && record.reauthorizes_revision.is_none()
        }));
        // Node 8 moves with its namespace; node 9 stays in the fleet.
        assert_eq!(registrations(8)[0].0, &new_build);
        assert_eq!(registrations(9)[0].0, &fleet);
        let binding: NamespaceBindingV1 = parse(
            plan.writes
                .iter()
                .find(|write| write.key == namespace_binding_key(&new_build))
                .unwrap(),
        );
        assert_eq!(binding.read_scopes, ["build".to_owned()].into());
    }

    #[test]
    fn republish_refuses_the_same_root_or_a_mixed_export() {
        let none = BTreeMap::new();
        assert!(plan_republish(&export(), &key(1), [20; 32], &none, 100).is_err());
        let mut mixed = export();
        mixed.issuers[0].issuer = hex::encode(key(3));
        assert!(plan_republish(&mixed, &key(2), [20; 32], &none, 100).is_err());
        let unbound = [(hex::encode([13; 32]), [21; 32])].into();
        assert!(plan_republish(&export(), &key(2), [20; 32], &unbound, 100).is_err());
    }
}
