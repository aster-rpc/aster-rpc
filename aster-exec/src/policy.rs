//! Exec grants from exact-root Docs records, with local denial watermarks.

use aster::Doc;
use aster::enrollment::{
    RootAuthoredReader, RootRecordRead, RootRevisionError, check_root_revision,
    should_record_rejection, validate_root_record_json,
};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

pub const EXEC_GRANT_PREFIX: &[u8] = b"aster/exec/v1/grants/";
pub const MAX_GRANT_BYTES: usize = 64 * 1024;
/// A grant issued at most this far in the future still applies, so modest
/// clock skew between the root and a target does not deny fresh grants.
pub const MAX_ISSUED_AT_SKEW_SECONDS: u64 = 60;
const GRANTS: TableDefinition<&str, &[u8]> = TableDefinition::new("exec_grants_v1");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("exec_grant_meta_v1");

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrantStatusV1 {
    Active,
    Revoked,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExecOperationV1 {
    Discover,
    Submit,
    Status,
    Output,
    Watch,
    Cancel,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecGrantV1 {
    pub version: u32,
    pub issuer: String,
    pub audience: String,
    pub caller: String,
    pub target: String,
    pub revision: u64,
    pub issued_at: u64,
    pub not_before: Option<u64>,
    pub expires_at: Option<u64>,
    pub status: GrantStatusV1,
    pub account_alias: String,
    pub profiles: Vec<String>,
    pub operations: Vec<ExecOperationV1>,
    pub cwd_roots: Vec<String>,
    pub max_timeout_ms: u64,
    pub max_output_bytes: u64,
    pub cancel_on_revocation: bool,
    pub reauthorizes_revision: Option<u64>,
    /// On-behalf-of submissions under this grant must carry an action chain
    /// whose leaf binds the exact request digest (single-use authority).
    #[serde(default)]
    pub require_request_digest: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    grant: Option<ExecGrantV1>,
    denial_watermark: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    version: u32,
    root: [u8; 32],
    namespace: [u8; 32],
    audience: String,
}

#[derive(Debug)]
pub enum GrantError {
    InvalidRecord,
    WrongContext,
    Stale,
    Conflict,
    ReauthorizationRequired,
    PolicyUnavailable,
    Denied,
    Storage(String),
}
impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Exec grant: {self:?}")
    }
}
impl std::error::Error for GrantError {}
fn storage(error: impl std::fmt::Display) -> GrantError {
    GrantError::Storage(error.to_string())
}
impl From<RootRevisionError> for GrantError {
    fn from(value: RootRevisionError) -> Self {
        match value {
            RootRevisionError::Stale => Self::Stale,
            RootRevisionError::Conflict => Self::Conflict,
            RootRevisionError::ReauthorizationRequired => Self::ReauthorizationRequired,
        }
    }
}

/// Parse a canonical node identity: exactly 64 lowercase hex characters.
pub fn parse_node_id(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    let mut bytes = [0; 32];
    hex::decode_to_slice(text, &mut bytes).ok()?;
    Some(bytes)
}

fn identity(text: &str) -> Result<[u8; 32], GrantError> {
    parse_node_id(text).ok_or(GrantError::InvalidRecord)
}

pub fn grant_key(caller: &[u8; 32], target: &[u8; 32]) -> Vec<u8> {
    let mut pair = [0u8; 64];
    pair[..32].copy_from_slice(caller);
    pair[32..].copy_from_slice(target);
    let mut key = EXEC_GRANT_PREFIX.to_vec();
    key.extend_from_slice(blake3::hash(&pair).to_hex().as_bytes());
    key
}

fn key_text(caller: &[u8; 32], target: &[u8; 32]) -> String {
    // The prefix and the BLAKE3 hex digest are both ASCII.
    grant_key(caller, target)
        .into_iter()
        .map(char::from)
        .collect()
}

/// Shared by concurrent refreshes: Docs reads need no lock, and each
/// read-modify-write of a stored grant runs under `apply`.
pub struct ExecGrantStore {
    database: redb::Database,
    reader: RootAuthoredReader,
    context: Context,
    authenticated: Mutex<BTreeSet<String>>,
    apply: Mutex<()>,
}

impl ExecGrantStore {
    /// The one audience every grant in this store carries.
    pub fn audience(&self) -> &str {
        &self.context.audience
    }

    pub fn open(
        path: &Path,
        root: [u8; 32],
        namespace: [u8; 32],
        audience: String,
    ) -> Result<Self, GrantError> {
        if audience.is_empty() {
            return Err(GrantError::WrongContext);
        }
        fs::create_dir_all(path.parent().ok_or(GrantError::WrongContext)?).map_err(storage)?;
        if !path.exists() {
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path).map_err(storage)?;
        }
        let database = redb::Database::create(path).map_err(storage)?;
        let context = Context {
            version: 1,
            root,
            namespace,
            audience,
        };
        let write = database.begin_write().map_err(storage)?;
        let mut meta = write.open_table(META).map_err(storage)?;
        let existing_context = meta
            .get("context")
            .map_err(storage)?
            .map(|value| value.value().to_vec());
        match existing_context {
            Some(existing) => {
                let saved: Context = serde_json::from_slice(&existing).map_err(storage)?;
                if saved.version != context.version
                    || saved.root != context.root
                    || saved.namespace != context.namespace
                    || saved.audience != context.audience
                {
                    return Err(GrantError::WrongContext);
                }
            }
            None => {
                let bytes = serde_json::to_vec(&context).map_err(storage)?;
                meta.insert("context", bytes.as_slice()).map_err(storage)?;
            }
        }
        drop(meta);
        write.open_table(GRANTS).map_err(storage)?;
        write.commit().map_err(storage)?;
        Ok(Self {
            database,
            reader: RootAuthoredReader::new(namespace, root, MAX_GRANT_BYTES as u64, 4096),
            context,
            authenticated: Mutex::new(BTreeSet::new()),
            apply: Mutex::new(()),
        })
    }

    fn read_stored(&self, key: &str) -> Result<Stored, GrantError> {
        let read = self.database.begin_read().map_err(storage)?;
        let table = read.open_table(GRANTS).map_err(storage)?;
        match table.get(key).map_err(storage)? {
            Some(value) => serde_json::from_slice(value.value()).map_err(storage),
            None => Ok(Stored {
                grant: None,
                denial_watermark: None,
            }),
        }
    }

    fn write_stored(&self, key: &str, stored: &Stored) -> Result<(), GrantError> {
        let bytes = serde_json::to_vec(stored).map_err(storage)?;
        let mut write = self.database.begin_write().map_err(storage)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(storage)?;
        write
            .open_table(GRANTS)
            .map_err(storage)?
            .insert(key, bytes.as_slice())
            .map_err(storage)?;
        write.commit().map_err(storage)
    }

    fn validate(&self, key: &[u8], grant: &ExecGrantV1) -> Result<(), GrantError> {
        let caller = identity(&grant.caller)?;
        let target = identity(&grant.target)?;
        if key != grant_key(&caller, &target)
            || grant.version != 1
            || identity(&grant.issuer)? != self.context.root
            || grant.audience != self.context.audience
            || grant.revision == 0
            || grant.issued_at > grant.expires_at.unwrap_or(u64::MAX)
            || grant
                .not_before
                .is_some_and(|nbf| grant.expires_at.is_some_and(|exp| nbf >= exp))
            || grant.account_alias.is_empty()
            || grant.account_alias.len() > 64
            || !grant
                .account_alias
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || grant.profiles.is_empty()
            || grant.operations.is_empty()
            || grant.cwd_roots.is_empty()
            || grant.profiles.len() > 32
            || grant.operations.len() > 6
            || grant.cwd_roots.len() > 32
            // First qualified backend always cancels on authority loss.
            // Reject an unsupported weaker policy rather than silently ignore it.
            || !grant.cancel_on_revocation
            || grant.max_timeout_ms == 0
            || grant.max_output_bytes == 0
        {
            return Err(GrantError::InvalidRecord);
        }
        if serde_json::to_vec(grant).map_err(storage)?.len() > MAX_GRANT_BYTES {
            return Err(GrantError::InvalidRecord);
        }
        Ok(())
    }

    fn observe(&self, key: String, grant: ExecGrantV1) -> Result<(), GrantError> {
        self.validate(key.as_bytes(), &grant)?;
        let mut stored = self.read_stored(&key)?;
        let old = stored.grant.as_ref();
        let old_needs_reauthorization = old.is_some_and(|old| {
            grant.status == GrantStatusV1::Active
                && (old.status != GrantStatusV1::Active
                    || old.expires_at.is_some_and(|exp| grant.issued_at >= exp))
        });
        let required_predecessor = if grant.status == GrantStatusV1::Active {
            old.filter(|_| old_needs_reauthorization)
                .map(|old| old.revision)
                .or(stored.denial_watermark)
        } else {
            None
        };
        let changed = check_root_revision(
            old.map(|old| old.revision),
            stored.denial_watermark,
            grant.revision,
            old == Some(&grant),
            required_predecessor,
            grant.reauthorizes_revision,
        )?;
        if changed {
            stored.grant = Some(grant);
            stored.denial_watermark = None;
            self.write_stored(&key, &stored)?;
        }
        self.authenticated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
        Ok(())
    }

    fn observe_invalid_binding(&self, key: &str, bytes: &[u8]) -> Result<(), GrantError> {
        #[derive(Deserialize)]
        struct Binding {
            issuer: String,
            audience: String,
            caller: String,
            target: String,
            revision: u64,
        }
        let Ok(binding) = serde_json::from_slice::<Binding>(bytes) else {
            return Ok(());
        };
        if identity(&binding.issuer).ok() != Some(self.context.root)
            || binding.audience != self.context.audience
            || identity(&binding.caller)
                .ok()
                .zip(identity(&binding.target).ok())
                .is_none_or(|(caller, target)| key.as_bytes() != grant_key(&caller, &target))
        {
            return Ok(());
        }
        let mut stored = self.read_stored(key)?;
        if should_record_rejection(
            stored.grant.as_ref().map(|g| g.revision),
            stored.denial_watermark,
            binding.revision,
        ) {
            stored.denial_watermark = Some(binding.revision);
            self.write_stored(key, &stored)?;
        }
        Ok(())
    }

    /// Read and apply the exact-root leaf for one caller and target.
    pub async fn refresh_one(
        &self,
        doc: &Doc,
        caller: &[u8; 32],
        target: &[u8; 32],
    ) -> Result<(), GrantError> {
        let read = self.read_leaf(doc, caller, target).await?;
        self.apply_leaf(caller, target, read)
    }

    /// Read the exact-root leaf from the local Docs replica. Changes nothing.
    pub async fn read_leaf(
        &self,
        doc: &Doc,
        caller: &[u8; 32],
        target: &[u8; 32],
    ) -> Result<RootRecordRead, GrantError> {
        self.reader
            .read_exact(doc, grant_key(caller, target))
            .await
            .map_err(storage)
    }

    /// Apply a leaf read. Every refresh reauthenticates the exact-root leaf:
    /// a missing, pending, unreadable or invalid leaf cannot renew an earlier
    /// positive authorization in this process. May fsync.
    pub fn apply_leaf(
        &self,
        caller: &[u8; 32],
        target: &[u8; 32],
        read: RootRecordRead,
    ) -> Result<(), GrantError> {
        let key = key_text(caller, target);
        let _apply = self.apply.lock().unwrap_or_else(|e| e.into_inner());
        self.authenticated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        let RootRecordRead::Available(bytes) = read else {
            return Ok(());
        };
        let grant = if validate_root_record_json(&bytes).is_ok() {
            serde_json::from_slice::<ExecGrantV1>(&bytes).ok()
        } else {
            None
        };
        let Some(grant) = grant else {
            self.observe_invalid_binding(&key, &bytes)?;
            return Err(GrantError::InvalidRecord);
        };
        if let Err(error) = self.observe(key.clone(), grant) {
            self.observe_invalid_binding(&key, &bytes)?;
            return Err(error);
        }
        Ok(())
    }

    /// The caller's usable grant. `Denied` is definite: no grant, a revoked
    /// or out-of-validity grant, or a newer root statement that could not be
    /// applied. `PolicyUnavailable` means the leaf was not reauthenticated in
    /// this process, so the answer is unknown.
    pub fn grant_for(
        &self,
        caller: &[u8; 32],
        target: &[u8; 32],
        now: u64,
    ) -> Result<ExecGrantV1, GrantError> {
        let key = key_text(caller, target);
        let stored = self.read_stored(&key)?;
        if stored.denial_watermark.is_some() {
            return Err(GrantError::Denied);
        }
        let grant = stored.grant.ok_or(GrantError::Denied)?;
        if !self
            .authenticated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            return Err(GrantError::PolicyUnavailable);
        }
        if grant.status != GrantStatusV1::Active
            || grant.issued_at > now.saturating_add(MAX_ISSUED_AT_SKEW_SECONDS)
            || grant.not_before.is_some_and(|nbf| now < nbf)
            || grant.expires_at.is_some_and(|exp| now >= exp)
        {
            return Err(GrantError::Denied);
        }
        Ok(grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(
        root: [u8; 32],
        caller: [u8; 32],
        target: [u8; 32],
        revision: u64,
        status: GrantStatusV1,
    ) -> ExecGrantV1 {
        ExecGrantV1 {
            version: 1,
            issuer: hex::encode(root),
            audience: "fleet".into(),
            caller: hex::encode(caller),
            target: hex::encode(target),
            revision,
            issued_at: revision,
            not_before: None,
            expires_at: None,
            status,
            account_alias: "builder".into(),
            profiles: vec!["open".into()],
            operations: vec![ExecOperationV1::Submit],
            cwd_roots: vec!["/srv/build".into()],
            max_timeout_ms: 1000,
            max_output_bytes: 4096,
            cancel_on_revocation: true,
            reauthorizes_revision: None,
            require_request_digest: false,
        }
    }

    #[test]
    fn denied_grant_requires_exact_newer_root_reauthorization() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("grants.redb");
        let (root, namespace, caller, target) = ([1; 32], [4; 32], [2; 32], [3; 32]);
        let key = key_text(&caller, &target);
        let store = ExecGrantStore::open(&path, root, namespace, "fleet".into()).unwrap();
        assert!(matches!(
            store.grant_for(&caller, &target, 2),
            Err(GrantError::Denied)
        ));
        let mut unsupported = grant(root, caller, target, 1, GrantStatusV1::Active);
        unsupported.cancel_on_revocation = false;
        assert!(matches!(
            store.validate(key.as_bytes(), &unsupported),
            Err(GrantError::InvalidRecord)
        ));
        store
            .observe(
                key.clone(),
                grant(root, caller, target, 1, GrantStatusV1::Active),
            )
            .unwrap();
        assert!(store.grant_for(&caller, &target, 2).is_ok());
        let revoked = grant(root, caller, target, 2, GrantStatusV1::Revoked);
        store.observe(key.clone(), revoked).unwrap();
        assert!(matches!(
            store.grant_for(&caller, &target, 2),
            Err(GrantError::Denied)
        ));
        let active = grant(root, caller, target, 3, GrantStatusV1::Active);
        assert!(matches!(
            store.observe(key.clone(), active.clone()),
            Err(GrantError::ReauthorizationRequired)
        ));
        store
            .observe_invalid_binding(&key, &serde_json::to_vec(&active).unwrap())
            .unwrap();
        // An unappliable newer root statement is a definite denial.
        assert!(matches!(
            store.grant_for(&caller, &target, 3),
            Err(GrantError::Denied)
        ));
        let mut active = grant(root, caller, target, 4, GrantStatusV1::Active);
        active.reauthorizes_revision = Some(2);
        store.observe(key.clone(), active).unwrap();
        // Issued at 4: a target clock a few seconds behind the root still
        // accepts it.
        assert!(store.grant_for(&caller, &target, 1).is_ok());
        drop(store);
        let reopened = ExecGrantStore::open(&path, root, namespace, "fleet".into()).unwrap();
        assert!(matches!(
            reopened.grant_for(&caller, &target, 4),
            Err(GrantError::PolicyUnavailable)
        ));
    }
}
