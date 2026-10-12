//! The shared Aster operator configuration, `~/.aster/config.toml`: profiles
//! (root public key, @aster handle, signer, published services) and the
//! @aster service settings. Any Aster tool reads and writes it through this
//! crate, so they agree on profiles.
//!
//! The file carries a format `version`. [`load`] migrates older formats in
//! memory, one step at a time ([`MIGRATIONS`]), and refuses newer ones; the
//! next save writes the current format. A file without `version` is format
//! 0, the layout the Python CLI writes. Keys this crate does not model
//! (another tool's table, a newer field) are kept on save. Updates take an
//! exclusive lock on `config.lock` around read-modify-write, so concurrent
//! tools do not lose each other's changes.
//!
//! `ASTER_HOME` overrides the directory (default `~/.aster`) and
//! `ASTER_PROFILE` the active profile.
//!
//! ```toml
//! version = 1
//! active_profile = "prod"
//!
//! [aster_service]
//! enabled = true
//! offline_banner = true
//!
//! [profiles.prod]
//! root_pubkey = "…"
//! email = "ops@example.com"
//! signer = "local"
//! published_services = ["svc-a", "svc-b"]
//! handle = { name = "ops", status = "verified", claimed_at = "2026-10-12T00:00:00Z" }
//! ```

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const HOME_ENV: &str = "ASTER_HOME";
pub const PROFILE_ENV: &str = "ASTER_PROFILE";
/// The format this crate writes.
pub const VERSION: i64 = 1;
const FILE: &str = "config.toml";
const LOCK: &str = "config.lock";

/// `ASTER_HOME` if set, else `~/.aster`; `None` only when neither exists.
pub fn default_home() -> Option<PathBuf> {
    match std::env::var_os(HOME_ENV) {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => std::env::home_dir().map(|h| h.join(".aster")),
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join(FILE)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Always [`VERSION`] after [`load`]; written as-is.
    #[serde(default)]
    pub version: i64,
    #[serde(default = "default_profile_name")]
    pub active_profile: String,
    #[serde(default)]
    pub aster_service: AsterService,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    /// Tables and keys owned by other tools or newer versions.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// The @aster directory service this machine talks to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsterService {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    #[serde(default = "yes")]
    pub offline_banner: bool,
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// One operator identity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Enrollment JWT issued for this profile's existing node key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_credential_file: Option<PathBuf>,
    /// Pinned enrollment policy namespace (public ID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_policy_namespace: Option<String>,
    /// Optional static direct addresses keyed by target node ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exec_direct_addresses: BTreeMap<String, Vec<String>>,
    /// Enrollment application/fleet audience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_audience: Option<String>,
    /// Hex Ed25519 root public key; the private key lives in the OS
    /// credential store, never here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_pubkey: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default)]
    pub signer: Signer,
    /// Sorted and unique; see [`Profile::set_published_services`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published_services: Vec<String>,
    /// The claimed @aster handle; absent when none has been claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<Handle>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// Where the root private key signs from. More kinds (KMS, remote, offline)
/// arrive as variants, with a format migration if they need parameters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signer {
    /// The OS credential store on this machine.
    #[default]
    Local,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handle {
    pub name: String,
    pub status: HandleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleStatus {
    /// Claimed, awaiting email verification.
    Pending,
    Verified,
}

fn default_profile_name() -> String {
    "default".into()
}
fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: VERSION,
            active_profile: default_profile_name(),
            aster_service: AsterService::default(),
            profiles: BTreeMap::new(),
            extra: toml::Table::new(),
        }
    }
}

impl Default for AsterService {
    fn default() -> Self {
        Self {
            enabled: true,
            node_id: None,
            relay: None,
            offline_banner: true,
            extra: toml::Table::new(),
        }
    }
}

impl Profile {
    /// Store sorted and de-duplicated.
    pub fn set_published_services<S: Into<String>>(
        &mut self,
        services: impl IntoIterator<Item = S>,
    ) {
        let set: std::collections::BTreeSet<String> =
            services.into_iter().map(Into::into).collect();
        self.published_services = set.into_iter().collect();
    }
}

impl Config {
    /// The profile in effect: `explicit` (a `--profile` flag), else
    /// `ASTER_PROFILE`, else `active_profile`. Not validated; see
    /// [`Config::selected_profile`].
    pub fn selected_profile_name(&self, explicit: Option<&str>) -> String {
        explicit
            .map(str::to_string)
            .or_else(|| std::env::var(PROFILE_ENV).ok().filter(|p| !p.is_empty()))
            .unwrap_or_else(|| self.active_profile.clone())
    }

    /// [`Config::selected_profile_name`], checked by [`validate_profile_name`].
    pub fn selected_profile(&self, explicit: Option<&str>) -> Result<String> {
        let name = self.selected_profile_name(explicit);
        validate_profile_name(&name)?;
        Ok(name)
    }
}

/// Read the configuration, migrated to [`VERSION`]; a missing file is the
/// default configuration.
pub fn load(home: &Path) -> Result<Config> {
    let path = path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Parse configuration text of any supported format version.
pub fn parse(text: &str) -> Result<Config> {
    let mut table: toml::Table = toml::from_str(text)?;
    let version = match table.get("version") {
        None => 0,
        Some(v) => v.as_integer().context("`version` must be an integer")?,
    };
    if version > VERSION {
        bail!(
            "format version {version} is newer than this tool supports ({VERSION}); upgrade Aster"
        );
    }
    let from = usize::try_from(version).context("`version` must not be negative")?;
    for migrate in &MIGRATIONS[from..] {
        migrate(&mut table)?;
    }
    table.insert("version".into(), VERSION.into());
    let mut config: Config = toml::Value::Table(table).try_into()?;
    for (name, profile) in &mut config.profiles {
        validate_profile_name(name).with_context(|| format!("profile `{name}`"))?;
        let services = std::mem::take(&mut profile.published_services);
        profile.set_published_services(services);
    }
    validate_profile_name(&config.active_profile).context("`active_profile`")?;
    Ok(config)
}

/// Profile names become file names and credential-store account names, so
/// every Aster tool accepts the same set: 1–64 characters from `a-z`, `0-9`,
/// `-` and `_`. Lowercase only, so names cannot collide on case-insensitive
/// file systems.
pub fn validate_profile_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'),
        "invalid profile name `{name}`: use 1-64 characters from a-z, 0-9, `-` and `_`"
    );
    Ok(())
}

/// `MIGRATIONS[n]` turns format `n` into format `n + 1`, in place.
pub const MIGRATIONS: [fn(&mut toml::Table) -> Result<()>; VERSION as usize] = [v0_to_v1];

/// Format 0 is the Python CLI's layout: empty strings for unset values,
/// `published_services` as one comma-separated string, and the handle as
/// three flat profile keys with status `unregistered` meaning none.
fn v0_to_v1(table: &mut toml::Table) -> Result<()> {
    use toml::Value;

    fn take_str(t: &mut toml::Table, key: &str) -> Option<String> {
        match t.remove(key) {
            Some(Value::String(s)) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
    fn put(t: &mut toml::Table, key: &str, value: Option<String>) {
        if let Some(v) = value {
            t.insert(key.into(), Value::String(v));
        }
    }

    if let Some(Value::Table(svc)) = table.get_mut("aster_service") {
        for key in ["node_id", "relay"] {
            let v = take_str(svc, key);
            put(svc, key, v);
        }
    }
    let Some(Value::Table(profiles)) = table.get_mut("profiles") else {
        return Ok(());
    };
    for (name, profile) in profiles.iter_mut() {
        let Value::Table(p) = profile else {
            bail!("profile `{name}` is not a table");
        };
        for key in ["root_pubkey", "email"] {
            let v = take_str(p, key);
            put(p, key, v);
        }
        if let Some(services) = take_str(p, "published_services") {
            let list: Vec<Value> = services
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.into()))
                .collect();
            p.insert("published_services".into(), Value::Array(list));
        }
        let name = take_str(p, "handle");
        let status = take_str(p, "handle_status");
        let claimed_at = take_str(p, "handle_claimed_at");
        if let (Some(name), Some(status)) = (name, status.filter(|s| s != "unregistered")) {
            let mut handle = toml::Table::new();
            handle.insert("name".into(), Value::String(name));
            handle.insert("status".into(), Value::String(status));
            put(&mut handle, "claimed_at", claimed_at);
            p.insert("handle".into(), Value::Table(handle));
        }
    }
    Ok(())
}

/// Read, change and save under the configuration lock. Nothing is written
/// if `change` fails. Generic over the error so callers keep their own error
/// type (and its classification) through the closure.
pub fn update<T, E: From<anyhow::Error>>(
    home: &Path,
    change: impl FnOnce(&mut Config) -> Result<T, E>,
) -> Result<T, E> {
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let _lock = lock(home)?;
    let mut config = load(home)?;
    let out = change(&mut config)?;
    save_locked(home, &config)?;
    Ok(out)
}

fn lock(home: &Path) -> Result<File> {
    let path = home.join(LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// Atomic replace with owner-only permissions on Unix. The caller holds the lock.
/// Written to an owner-only temporary file and flushed to disk before the
/// rename, so a crash leaves the old file or the new one, never an empty one:
/// the file records each profile's root public key.
fn save_locked(home: &Path, config: &Config) -> Result<()> {
    use std::io::Write;
    let path = path(home);
    let write = || -> Result<()> {
        let tmp = path.with_extension("toml.tmp");
        let mut config = config.clone();
        config.version = VERSION;
        let mut open = OpenOptions::new();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        let mut file = open.open(&tmp)?;
        file.write_all(toml::to_string(&config)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        #[cfg(unix)]
        File::open(home)?.sync_all()?;
        Ok(())
    };
    write().with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file as the Python CLI writes it, plus the shell's `[shell]` table
    /// and a profile field this crate does not model.
    const PYTHON: &str = r#"active_profile = "prod"

[aster_service]
enabled = false
node_id = "abc"
relay = ""
offline_banner = true

[profiles.prod]
root_pubkey = "00ff"
handle = "emrul"
handle_status = "verified"
handle_claimed_at = "2026-04-11T00:00:00Z"
email = "a@example.com"
signer = "local"
published_services = "b,a"
display_name = "Emrul"
announcements = true

[shell]
first_time = false
"#;

    fn temp_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aster-config-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migrates_python_file_and_keeps_unknown_keys() {
        let home = temp_home("roundtrip");
        std::fs::write(path(&home), PYTHON).unwrap();
        let cfg = load(&home).unwrap();
        assert_eq!(cfg.version, VERSION);
        assert_eq!(cfg.active_profile, "prod");
        assert!(!cfg.aster_service.enabled);
        assert_eq!(cfg.aster_service.node_id.as_deref(), Some("abc"));
        assert_eq!(cfg.aster_service.relay, None);
        let prod = &cfg.profiles["prod"];
        assert_eq!(
            prod.handle,
            Some(Handle {
                name: "emrul".into(),
                status: HandleStatus::Verified,
                claimed_at: Some("2026-04-11T00:00:00Z".into()),
            })
        );
        assert_eq!(prod.published_services, ["a", "b"], "sorted on load");

        update(&home, |c| -> Result<()> {
            c.profiles.get_mut("prod").unwrap().email = Some("b@example.com".into());
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(path(&home)).unwrap();
        assert!(written.starts_with("version = 1\n"), "{written}");
        assert!(!written.contains("handle_status"), "{written}");
        let again = load(&home).unwrap();
        let prod = &again.profiles["prod"];
        assert_eq!(prod.email.as_deref(), Some("b@example.com"));
        assert_eq!(prod.extra["display_name"].as_str(), Some("Emrul"));
        assert_eq!(again.extra["shell"]["first_time"].as_bool(), Some(false));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn unregistered_python_profile_has_no_handle() {
        let cfg = parse("[profiles.dev]\nhandle = \"\"\nhandle_status = \"unregistered\"\nemail = \"\"\npublished_services = \"\"\n").unwrap();
        assert_eq!(cfg.profiles["dev"], Profile::default());
    }

    #[test]
    fn profile_names_follow_one_rule() {
        assert!(parse("[profiles.Prod]\n").is_err(), "uppercase");
        assert!(parse("version = 1\nactive_profile = \"a b\"\n").is_err());
        assert!(parse("version = 1\n[profiles.ok-1_x]\n").is_ok());
        assert!(validate_profile_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn newer_format_is_refused() {
        let err = parse("version = 2\n").unwrap_err().to_string();
        assert!(err.contains("newer"), "{err}");
    }

    #[test]
    fn unknown_signer_is_an_error() {
        assert!(parse("version = 1\n[profiles.p]\nsigner = \"kms\"\n").is_err());
    }

    #[test]
    fn missing_file_is_current_default() {
        let home = temp_home("defaults");
        let cfg = load(&home).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(cfg.aster_service.enabled && cfg.aster_service.offline_banner);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn failed_change_writes_nothing() {
        let home = temp_home("failed");
        let r: Result<()> = update(&home, |c| {
            c.active_profile = "x".into();
            anyhow::bail!("refused")
        });
        assert!(r.is_err());
        assert!(!path(&home).exists());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let home = temp_home("concurrent");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let home = home.clone();
                std::thread::spawn(move || {
                    update(&home, |c| -> Result<()> {
                        c.profiles.insert(format!("p{i}"), Profile::default());
                        Ok(())
                    })
                    .unwrap()
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(load(&home).unwrap().profiles.len(), 8);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn published_services_sorted_and_unique() {
        let mut p = Profile::default();
        p.set_published_services(["svc-b", "svc-a", "svc-b"]);
        assert_eq!(p.published_services, ["svc-a", "svc-b"]);
    }
}
