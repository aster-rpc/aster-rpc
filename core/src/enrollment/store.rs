//! Durable, issuer/namespace-pinned policy. The local watermark survives
//! process restart and stale replicas, but cannot detect rollback of the entire
//! disk including this file. Protect that boundary with an external monotonic
//! anchor when the application requires whole-machine rollback resistance.
use super::jwt::{validate_unique_json, RegistrationEvidenceData};
use super::records::*;
use super::root_records::{RootAuthoredDocView, RootRecordError, RootRecordRead};
use super::root_revisions::{check_root_revision, should_record_rejection, RootRevisionError};
use super::{AdmissionEvidenceSource, AdmissionGuard, DenialReason, VerifiedAdmissionEvidence};
use crate::CoreDoc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    InvalidConfiguration,
    WrongNamespace,
    WrongAudience,
    InvalidRecord,
    UnsupportedVersion,
    UnsupportedMandatoryRestriction,
    StaleRevision,
    ConflictingRevision,
    ReauthorizationRequired,
    MissingEvidence,
    Denied(DenialReason),
    TooManyRecords,
    Storage(String),
}
impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "enrollment policy: {self:?}")
    }
}
impl std::error::Error for RecordError {}
fn storage(error: impl fmt::Display) -> RecordError {
    RecordError::Storage(error.to_string())
}
impl From<RootRecordError> for RecordError {
    fn from(value: RootRecordError) -> Self {
        match value {
            RootRecordError::WrongNamespace => Self::WrongNamespace,
            RootRecordError::TooManyRecords => Self::TooManyRecords,
            RootRecordError::OversizedRecord => Self::InvalidRecord,
            RootRecordError::InvalidJson => Self::InvalidRecord,
            RootRecordError::Storage(error) => Self::Storage(error),
        }
    }
}
impl From<RootRevisionError> for RecordError {
    fn from(value: RootRevisionError) -> Self {
        match value {
            RootRevisionError::Stale => Self::StaleRevision,
            RootRevisionError::Conflict => Self::ConflictingRevision,
            RootRevisionError::ReauthorizationRequired => Self::ReauthorizationRequired,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyRefreshReport {
    /// Entries whose value bytes were locally complete, including rejected values.
    pub available: usize,
    pub pending: usize,
    /// Records for another audience are unrelated to this policy view.
    pub ignored: usize,
    /// Leaf failures are isolated so they cannot hide later root revocations.
    pub rejected: Vec<PolicyRefreshIssue>,
}
impl PolicyRefreshReport {
    pub fn has_uncertainty(&self) -> bool {
        self.pending > 0 || !self.rejected.is_empty()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRefreshIssue {
    pub key: Vec<u8>,
    pub error: RecordError,
}

enum PolicyRecord {
    Node(NodeRegistrationV1),
    Credential(CredentialStatusV1),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPolicy {
    version: u32,
    root: [u8; 32],
    namespace: [u8; 32],
    audience: String,
    nodes: BTreeMap<String, NodeRegistrationV1>,
    /// Denial-only watermark; never reconstructs positive authority.
    #[serde(default)]
    rejected_nodes: BTreeMap<String, u64>,
    credentials: BTreeMap<String, CredentialStatusV1>,
}
const MAX_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;
#[derive(Clone)]
struct State {
    /// Positive authority is reconstructed only by authenticated local Docs reads.
    authenticated_nodes: BTreeSet<String>,
    policy: PersistedPolicy,
    failed: bool,
}

/// One owner per path is enforced with an OS advisory file lock, held for the
/// store's lifetime. Writes sync the file and atomically replace it before
/// returning; Unix also syncs the containing directory. Windows does not claim
/// directory-entry durability across power loss.
/// Persisted positives are watermarks only until [`Self::refresh_node`] or
/// [`Self::refresh`] reauthenticates the corresponding exact-root local Docs
/// record. This requires no network, so persistent Docs supports offline reboot.
/// Local denial/watermark files remain trusted application state; replacing or
/// restoring both stores is outside the local rollback guarantee.
pub struct EnrollmentPolicyStore {
    path: PathBuf,
    _lock: File,
    state: Mutex<State>,
    root: [u8; 32],
    namespace: [u8; 32],
    audience: String,
}
impl EnrollmentPolicyStore {
    pub fn open(
        path: impl AsRef<Path>,
        root: [u8; 32],
        namespace: [u8; 32],
        audience: impl Into<String>,
    ) -> Result<Self, RecordError> {
        let audience = audience.into();
        if super::JwtEnrollmentVerifier::new(root, audience.clone()).is_err() {
            return Err(RecordError::InvalidConfiguration);
        }
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(storage)?;
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = private_open(Path::new(&lock_path), false)?;
        lock.try_lock().map_err(storage)?;
        let policy = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_SNAPSHOT_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(storage)?;
                if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
                    return Err(RecordError::TooManyRecords);
                }
                validate_unique_json(&bytes).map_err(|_| RecordError::InvalidRecord)?;
                serde_json::from_slice::<PersistedPolicy>(&bytes).map_err(storage)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedPolicy {
                version: 1,
                root,
                namespace,
                audience: audience.clone(),
                nodes: BTreeMap::new(),
                rejected_nodes: BTreeMap::new(),
                credentials: BTreeMap::new(),
            },
            Err(error) => return Err(storage(error)),
        };
        if policy.nodes.len() + policy.credentials.len() > MAX_POLICY_RECORDS
            || policy.rejected_nodes.len() > MAX_POLICY_RECORDS
        {
            return Err(RecordError::TooManyRecords);
        }
        if policy.version != 1
            || policy.root != root
            || policy.namespace != namespace
            || policy.audience != audience
        {
            return Err(RecordError::InvalidConfiguration);
        }
        let store = Self {
            path,
            _lock: lock,
            state: Mutex::new(State {
                authenticated_nodes: BTreeSet::new(),
                policy,
                failed: false,
            }),
            root,
            namespace,
            audience,
        };
        {
            let state = store.state.lock().unwrap_or_else(|e| e.into_inner());
            for (key, record) in &state.policy.nodes {
                store.validate_node(record)?;
                if key != &record.subject {
                    return Err(RecordError::InvalidRecord);
                }
            }
            for (subject, revision) in &state.policy.rejected_nodes {
                identity(subject)?;
                if *revision == 0
                    || state
                        .policy
                        .nodes
                        .get(subject)
                        .is_some_and(|node| node.revision > *revision)
                {
                    return Err(RecordError::InvalidRecord);
                }
            }
            for (key, record) in &state.policy.credentials {
                store.validate_credential(record)?;
                if key != &record.credential_id {
                    return Err(RecordError::InvalidRecord);
                }
            }
            store.persist(&state.policy)?;
        }
        Ok(store)
    }
    pub fn root(&self) -> &[u8; 32] {
        &self.root
    }
    pub fn namespace(&self) -> &[u8; 32] {
        &self.namespace
    }
    pub fn audience(&self) -> &str {
        &self.audience
    }
    /// Highest accepted registration revision, excluding rejected-statement
    /// barriers. Revisions are not a fleet-wide sequence number.
    pub fn observed_node_revision(&self, subject: &[u8; 32]) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .nodes
            .get(&hex::encode(subject))
            .map(|node| node.revision)
    }
    /// Highest rejected statement blocking this subject, when present. A root
    /// recovery statement must use a strictly higher revision than this barrier.
    pub fn rejected_node_revision(&self, subject: &[u8; 32]) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .rejected_nodes
            .get(&hex::encode(subject))
            .copied()
    }
    pub fn observed_credential_revision(&self, id: &str) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .credentials
            .get(id)
            .map(|record| record.revision)
    }
    fn check_doc(&self, doc: &CoreDoc) -> Result<(), RecordError> {
        if doc.doc.id().as_bytes() != &self.namespace {
            Err(RecordError::WrongNamespace)
        } else {
            Ok(())
        }
    }
    async fn read(
        &self,
        doc: &CoreDoc,
        key: Vec<u8>,
    ) -> Result<(RecordAvailability, Option<Vec<u8>>), RecordError> {
        let view = RootAuthoredDocView::new(
            self.namespace,
            self.root,
            MAX_POLICY_RECORD_BYTES as u64,
            MAX_POLICY_RECORDS,
        );
        Ok(match view.read_exact(doc, key).await? {
            RootRecordRead::Absent => (RecordAvailability::Absent, None),
            RootRecordRead::Pending => (RecordAvailability::Pending, None),
            RootRecordRead::Available(value) => (RecordAvailability::Available, Some(value)),
        })
    }
    pub async fn refresh_node(
        &self,
        doc: &CoreDoc,
        peer: &[u8; 32],
    ) -> Result<RecordAvailability, RecordError> {
        let key = node_registration_key(peer);
        let (availability, value) = self.read(doc, key.clone()).await?;
        if let Some(value) = value {
            match self.decode_record(&key, &value) {
                Ok(PolicyRecord::Node(record)) => self.observe_node(record)?,
                Ok(PolicyRecord::Credential(_)) => return Err(RecordError::InvalidRecord),
                Err(error) => {
                    if let Some((subject, revision)) = self.rejection_watermark(&key, &value) {
                        self.observe_rejection(subject, revision)?;
                    }
                    return Err(error);
                }
            }
        }
        Ok(availability)
    }
    pub async fn refresh_credential(
        &self,
        doc: &CoreDoc,
        id: &str,
    ) -> Result<RecordAvailability, RecordError> {
        if id.is_empty() || id.len() > 256 {
            return Err(RecordError::InvalidRecord);
        }
        let (availability, value) = self.read(doc, credential_status_key(id)).await?;
        if let Some(value) = value {
            validate_unique_json(&value).map_err(|_| RecordError::InvalidRecord)?;
            let record: CredentialStatusV1 =
                serde_json::from_slice(&value).map_err(|_| RecordError::InvalidRecord)?;
            if record.credential_id != id {
                return Err(RecordError::InvalidRecord);
            }
            self.observe_credential(record)?;
        }
        Ok(availability)
    }
    /// Refresh exact-root leaves independently, then commit all accepted changes
    /// in one durable snapshot. A wrong-audience or unusable record cannot hide
    /// a later revocation. Missing/tombstoned/pending values preserve cached
    /// state. Inspect [`PolicyRefreshReport::has_uncertainty`] before claiming
    /// content-complete synchronization; this local refresh alone proves no sync.
    pub async fn refresh(&self, doc: &CoreDoc) -> Result<PolicyRefreshReport, RecordError> {
        self.check_doc(doc)?;
        let view = RootAuthoredDocView::new(
            self.namespace,
            self.root,
            MAX_POLICY_RECORD_BYTES as u64,
            MAX_POLICY_RECORDS,
        );
        let entries = view.scan_prefix(doc, POLICY_RECORD_PREFIX).await?;
        let mut report = PolicyRefreshReport::default();
        let mut records = Vec::new();
        let mut rejected_nodes = Vec::new();
        for (key, read_result) in entries {
            let result = async {
                let value = match read_result? {
                    RootRecordRead::Available(value) => value,
                    RootRecordRead::Pending | RootRecordRead::Absent => {
                        report.pending += 1;
                        return Ok(None);
                    }
                };
                report.available += 1;
                match self.decode_record(&key, &value) {
                    Ok(record) => Ok(Some(record)),
                    Err(error) => {
                        if let Some(watermark) = self.rejection_watermark(&key, &value) {
                            rejected_nodes.push(watermark);
                        }
                        Err(error)
                    }
                }
            }
            .await;
            match result {
                Ok(Some(record)) => records.push((key, record)),
                Ok(None) => {}
                Err(RecordError::WrongAudience) => report.ignored += 1,
                Err(error) => report.rejected.push(PolicyRefreshIssue { key, error }),
            }
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::check_state(&state)?;
        let mut next = state.clone();
        let mut changed = false;
        let mut capacity_failure = false;
        for (key, record) in records {
            let watermark = match &record {
                PolicyRecord::Node(record) => Some((record.subject.clone(), record.revision)),
                PolicyRecord::Credential(_) => None,
            };
            let result = match record {
                PolicyRecord::Node(record) => self.apply_node(&mut next, record),
                PolicyRecord::Credential(record) => self.apply_credential(&mut next, record),
            };
            match result {
                Ok(update) => changed |= update,
                Err(RecordError::WrongAudience) => report.ignored += 1,
                Err(error) => {
                    capacity_failure |= error == RecordError::TooManyRecords;
                    if let Some(watermark) = watermark {
                        rejected_nodes.push(watermark);
                    }
                    report.rejected.push(PolicyRefreshIssue { key, error });
                }
            }
        }
        for (subject, revision) in rejected_nodes {
            match Self::apply_rejection(&mut next, subject, revision) {
                Ok(update) => changed |= update,
                Err(_) => capacity_failure = true,
            }
        }
        self.commit(&mut state, next, changed)?;
        if capacity_failure {
            state.failed = true;
            return Err(RecordError::TooManyRecords);
        }
        Ok(report)
    }
    fn decode_record(&self, key: &[u8], value: &[u8]) -> Result<PolicyRecord, RecordError> {
        validate_unique_json(value).map_err(|_| RecordError::InvalidRecord)?;
        if key.starts_with(b"aster/enrollment/v1/nodes/") {
            // Scope before interpreting a versioned schema: a future schema for
            // another service sharing this namespace is unrelated, not an
            // incomplete update to this audience's policy.
            #[derive(Deserialize)]
            struct ScopedEnvelope {
                audience: super::Audience,
            }
            let scope: ScopedEnvelope =
                serde_json::from_slice(value).map_err(|_| RecordError::InvalidRecord)?;
            if !scope.audience.contains(&self.audience) {
                return Err(RecordError::WrongAudience);
            }
            let record: NodeRegistrationV1 =
                serde_json::from_slice(value).map_err(|_| RecordError::InvalidRecord)?;
            if key != node_registration_key(&identity(&record.subject)?) {
                return Err(RecordError::InvalidRecord);
            }
            self.validate_node(&record)?;
            Ok(PolicyRecord::Node(record))
        } else if key.starts_with(b"aster/enrollment/v1/credentials/") {
            let record: CredentialStatusV1 =
                serde_json::from_slice(value).map_err(|_| RecordError::InvalidRecord)?;
            if key != credential_status_key(&record.credential_id) {
                return Err(RecordError::InvalidRecord);
            }
            self.validate_credential(&record)?;
            Ok(PolicyRecord::Credential(record))
        } else {
            Err(RecordError::InvalidRecord)
        }
    }
    // Extract only reliable denial metadata from an authenticated root leaf.
    // An unrecognized payload without these bindings stays ordinary uncertainty.
    fn rejection_watermark(&self, key: &[u8], value: &[u8]) -> Option<(String, u64)> {
        validate_unique_json(value).ok()?;
        #[derive(Deserialize)]
        struct Binding {
            issuer: String,
            subject: String,
            audience: super::Audience,
            revision: u64,
        }
        let binding: Binding = serde_json::from_slice(value).ok()?;
        if binding.revision == 0
            || identity(&binding.issuer).ok()? != self.root
            || !binding.audience.contains(&self.audience)
        {
            return None;
        }
        let subject = identity(&binding.subject).ok()?;
        if key != node_registration_key(&subject) {
            return None;
        }
        Some((binding.subject, binding.revision))
    }
    fn apply_rejection(
        state: &mut State,
        subject: String,
        revision: u64,
    ) -> Result<bool, RecordError> {
        if !should_record_rejection(
            state.policy.nodes.get(&subject).map(|node| node.revision),
            state.policy.rejected_nodes.get(&subject).copied(),
            revision,
        ) {
            return Ok(false);
        }
        if !state.policy.rejected_nodes.contains_key(&subject)
            && state.policy.rejected_nodes.len() >= MAX_POLICY_RECORDS
        {
            return Err(RecordError::TooManyRecords);
        }
        state.policy.rejected_nodes.insert(subject, revision);
        Ok(true)
    }
    fn observe_rejection(&self, subject: String, revision: u64) -> Result<(), RecordError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::check_state(&state)?;
        if !should_record_rejection(
            state.policy.nodes.get(&subject).map(|node| node.revision),
            state.policy.rejected_nodes.get(&subject).copied(),
            revision,
        ) {
            return Ok(());
        }
        let mut next = state.clone();
        match Self::apply_rejection(&mut next, subject, revision) {
            Ok(changed) => self.commit(&mut state, next, changed),
            Err(error) => {
                state.failed = true;
                Err(error)
            }
        }
    }
    fn validate_node(&self, record: &NodeRegistrationV1) -> Result<(), RecordError> {
        if !record.audience.contains(&self.audience) {
            return Err(RecordError::WrongAudience);
        }
        if record.version != POLICY_RECORD_VERSION {
            return Err(RecordError::UnsupportedVersion);
        }
        if identity(&record.issuer)? != self.root || record.revision == 0 {
            return Err(RecordError::InvalidRecord);
        }
        if !record.mandatory.is_empty() {
            return Err(RecordError::UnsupportedMandatoryRestriction);
        }
        VerifiedAdmissionEvidence::from_registration(RegistrationEvidenceData {
            issuer: self.root,
            subject: identity(&record.subject)?,
            audience: record.audience.clone(),
            issued_at: record.issued_at,
            not_before: record.not_before,
            expires_at: record.expires_at,
            claims: record.claims.clone(),
            document_id: self.namespace,
            revision: record.revision,
        })
        .map_err(|_| RecordError::InvalidRecord)?;
        if serde_json::to_vec(record).map_err(storage)?.len() > MAX_POLICY_RECORD_BYTES {
            return Err(RecordError::InvalidRecord);
        }
        Ok(())
    }
    fn validate_credential(&self, record: &CredentialStatusV1) -> Result<(), RecordError> {
        if record.version != 1 {
            return Err(RecordError::UnsupportedVersion);
        }
        if identity(&record.issuer)? != self.root
            || record.revision == 0
            || record.credential_id.is_empty()
            || record.credential_id.len() > 256
        {
            return Err(RecordError::InvalidRecord);
        }
        Ok(())
    }
    // Private: only authenticated Docs reads feed these transitions.
    fn observe_node(&self, record: NodeRegistrationV1) -> Result<(), RecordError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::check_state(&state)?;
        let changed = match Self::check_node_update(&state, &record) {
            Ok(changed) => changed,
            Err(error) => {
                if error == RecordError::TooManyRecords {
                    state.failed = true;
                }
                if state
                    .policy
                    .nodes
                    .get(&record.subject)
                    .is_some_and(|node| node.revision > record.revision)
                    || state
                        .policy
                        .rejected_nodes
                        .get(&record.subject)
                        .is_some_and(|known| *known >= record.revision)
                {
                    return Err(error);
                }
                let mut next = state.clone();
                match Self::apply_rejection(&mut next, record.subject.clone(), record.revision) {
                    Ok(changed) => self.commit(&mut state, next, changed)?,
                    Err(_) => state.failed = true,
                }
                return Err(error);
            }
        };
        if !changed {
            state.authenticated_nodes.insert(record.subject);
            return Ok(());
        }
        let mut next = state.clone();
        self.apply_node(&mut next, record)?;
        self.commit(&mut state, next, changed)
    }
    fn observe_credential(&self, record: CredentialStatusV1) -> Result<(), RecordError> {
        self.validate_credential(&record)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::check_state(&state)?;
        let changed = match Self::check_credential_update(&state, &record) {
            Ok(changed) => changed,
            Err(error) => {
                if error == RecordError::TooManyRecords {
                    state.failed = true;
                }
                return Err(error);
            }
        };
        if !changed {
            return Ok(());
        }
        let mut next = state.clone();
        self.apply_credential(&mut next, record)?;
        self.commit(&mut state, next, changed)
    }
    fn check_state(state: &State) -> Result<(), RecordError> {
        if state.failed {
            Err(RecordError::Storage("previous persistence failure".into()))
        } else {
            Ok(())
        }
    }
    fn check_node_update(state: &State, record: &NodeRegistrationV1) -> Result<bool, RecordError> {
        let old = state.policy.nodes.get(&record.subject);
        // Renewal is judged by the root's signed issuance time, not when an
        // offline replica eventually learns it.
        let old_requires_reauthorization = old.is_some_and(|old| {
            record.status == NodeRegistrationStatus::Active
                && (old.status != NodeRegistrationStatus::Active
                    || old.expires_at.is_some_and(|exp| record.issued_at >= exp))
        });
        let denial = state.policy.rejected_nodes.get(&record.subject).copied();
        let required_predecessor = if record.status == NodeRegistrationStatus::Active {
            old_requires_reauthorization.then(|| old.unwrap().revision)
        } else {
            None
        };
        let changed = check_root_revision(
            old.map(|old| old.revision),
            denial,
            record.revision,
            old == Some(record),
            required_predecessor,
            record.reauthorizes_revision,
        )?;
        if !changed {
            return Ok(false);
        }
        if !state.policy.nodes.contains_key(&record.subject)
            && state.policy.nodes.len() + state.policy.credentials.len() >= MAX_POLICY_RECORDS
        {
            return Err(RecordError::TooManyRecords);
        }
        Ok(true)
    }
    fn apply_node(
        &self,
        state: &mut State,
        record: NodeRegistrationV1,
    ) -> Result<bool, RecordError> {
        let changed = Self::check_node_update(state, &record)?;
        state.authenticated_nodes.insert(record.subject.clone());
        if changed {
            state.policy.rejected_nodes.remove(&record.subject);
            state.policy.nodes.insert(record.subject.clone(), record);
        }
        Ok(changed)
    }
    fn check_credential_update(
        state: &State,
        record: &CredentialStatusV1,
    ) -> Result<bool, RecordError> {
        let old = state.policy.credentials.get(&record.credential_id);
        let changed = check_root_revision(
            old.map(|old| old.revision),
            None,
            record.revision,
            old == Some(record),
            old.filter(|old| old.revoked && !record.revoked)
                .map(|old| old.revision),
            record.reauthorizes_revision,
        )?;
        if !changed {
            return Ok(false);
        }
        if !state.policy.credentials.contains_key(&record.credential_id)
            && state.policy.nodes.len() + state.policy.credentials.len() >= MAX_POLICY_RECORDS
        {
            return Err(RecordError::TooManyRecords);
        }
        Ok(true)
    }
    fn apply_credential(
        &self,
        state: &mut State,
        record: CredentialStatusV1,
    ) -> Result<bool, RecordError> {
        let changed = Self::check_credential_update(state, &record)?;
        if changed {
            state
                .policy
                .credentials
                .insert(record.credential_id.clone(), record);
        }
        Ok(changed)
    }
    fn commit(&self, state: &mut State, next: State, changed: bool) -> Result<(), RecordError> {
        if changed {
            if let Err(error) = self.persist(&next.policy) {
                state.failed = true;
                return Err(error);
            }
        }
        *state = next;
        Ok(())
    }
    /// Registration takes precedence whenever one has been observed. A known
    /// revoked, superseded, not-yet-valid or expired registration never falls
    /// back to an older credential. Absence alone permits credential admission.
    pub fn select_evidence(
        &self,
        peer: &[u8; 32],
        credential: Option<&VerifiedAdmissionEvidence>,
        now: u64,
    ) -> Result<VerifiedAdmissionEvidence, RecordError> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Err(RecordError::Denied(DenialReason::PolicyStateUnavailable));
        }
        if state.policy.rejected_nodes.contains_key(&hex::encode(peer)) {
            return Err(RecordError::Denied(DenialReason::PolicyStateUnavailable));
        }
        let selected = if let Some(record) = state.policy.nodes.get(&hex::encode(peer)) {
            if record.status != NodeRegistrationStatus::Active {
                return Err(RecordError::Denied(DenialReason::NodeRevoked));
            }
            if record.issued_at > now
                || record.not_before.is_some_and(|nbf| now < nbf)
                || record.expires_at.is_some_and(|exp| now >= exp)
            {
                return Err(RecordError::Denied(DenialReason::InvalidValidity));
            }
            if !state.authenticated_nodes.contains(&record.subject) {
                return Err(RecordError::MissingEvidence);
            }
            VerifiedAdmissionEvidence::from_registration(RegistrationEvidenceData {
                issuer: self.root,
                subject: *peer,
                audience: record.audience.clone(),
                issued_at: record.issued_at,
                not_before: record.not_before,
                expires_at: record.expires_at,
                claims: record.claims.clone(),
                document_id: self.namespace,
                revision: record.revision,
            })
            .map_err(|_| RecordError::InvalidRecord)?
        } else {
            credential.cloned().ok_or(RecordError::MissingEvidence)?
        };
        drop(state);
        if selected.subject() != peer
            || selected.issuer() != &self.root
            || !selected.audience().contains(&self.audience)
        {
            return Err(RecordError::InvalidRecord);
        }
        if let Some(reason) = self.denial(&selected, now) {
            return Err(RecordError::Denied(reason));
        }
        if !selected.valid_at(now) {
            return Err(RecordError::Denied(DenialReason::InvalidValidity));
        }
        Ok(selected)
    }
    fn persist(&self, policy: &PersistedPolicy) -> Result<(), RecordError> {
        let bytes = serde_json::to_vec(policy).map_err(storage)?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(RecordError::TooManyRecords);
        }
        let mut random = [0; 16];
        getrandom::getrandom(&mut random).map_err(storage)?;
        let tmp = self
            .path
            .with_extension(format!("pending-{}", hex::encode(random)));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(storage)?;
        let result = (|| {
            file.write_all(&bytes).map_err(storage)?;
            file.sync_all().map_err(storage)?;
            std::fs::rename(&tmp, &self.path).map_err(storage)?;
            #[cfg(unix)]
            {
                let parent = self
                    .path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                File::open(parent)
                    .and_then(|f| f.sync_all())
                    .map_err(storage)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}
impl AdmissionGuard for EnrollmentPolicyStore {
    fn denial(&self, evidence: &VerifiedAdmissionEvidence, now: u64) -> Option<DenialReason> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed {
            return Some(DenialReason::PolicyStateUnavailable);
        }
        if evidence.issuer() != &self.root || !evidence.audience().contains(&self.audience) {
            return Some(DenialReason::MissingEvidence);
        }
        if state
            .policy
            .rejected_nodes
            .contains_key(&hex::encode(evidence.subject()))
        {
            return Some(DenialReason::PolicyStateUnavailable);
        }
        if let Some(node) = state.policy.nodes.get(&hex::encode(evidence.subject())) {
            if node.status != NodeRegistrationStatus::Active {
                return Some(DenialReason::NodeRevoked);
            }
            if node.issued_at > now
                || node.not_before.is_some_and(|nbf| now < nbf)
                || node.expires_at.is_some_and(|exp| now >= exp)
            {
                return Some(DenialReason::InvalidValidity);
            }
            if !state.authenticated_nodes.contains(&node.subject) {
                return Some(DenialReason::MissingEvidence);
            }
            if evidence.source()
                != (AdmissionEvidenceSource::RootAuthoredRecord {
                    document_id: self.namespace,
                    revision: node.revision,
                })
                || evidence.claims() != &node.claims
                || evidence.audience() != &node.audience
                || evidence.not_before() != node.not_before
                || evidence.issued_at() != node.issued_at
                || evidence.expires_at() != node.expires_at
            {
                return Some(DenialReason::RegistrationSuperseded);
            }
        } else if matches!(
            evidence.source(),
            AdmissionEvidenceSource::RootAuthoredRecord { .. }
        ) {
            return Some(DenialReason::MissingEvidence);
        }
        if evidence.credential_id().is_some_and(|id| {
            state
                .policy
                .credentials
                .get(id)
                .is_some_and(|status| status.revoked || status.issued_at > now)
        }) {
            return Some(DenialReason::CredentialRevoked);
        }
        None
    }
}
fn identity(value: &str) -> Result<[u8; 32], RecordError> {
    super::canonical_identity(value).map_err(|_| RecordError::InvalidRecord)
}
fn private_open(path: &Path, truncate: bool) -> Result<File, RecordError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(storage)
}
