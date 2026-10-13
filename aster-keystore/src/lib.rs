//! Where Aster operator tools keep each profile's root private key (an
//! Ed25519 seed). Two stores:
//!
//! - **OS**: the macOS Keychain or Windows Credential Manager, service
//!   `aster`, account `root-key/<profile>`. The default on those platforms.
//! - **File**: `<home>/keys/<profile>.root.key`, 64 hex characters, in an
//!   owner-only directory (0700) and file (0600). The default elsewhere, and
//!   what headless hosts use.
//!
//! Each profile has two slots ([`Slot`]): the active root, and the next root
//! a pre-rotation commitment names (account `next-root-key/<profile>`, file
//! `<profile>.next-root.key`). The next root is meant to leave this machine
//! once committed; see `aster trust`.
//!
//! `ASTER_KEYSTORE=os|file` overrides the choice and `ASTER_KEYRING_SERVICE`
//! the OS service name; tests set both so they never touch real entries.
//!
//! **Trust boundary on macOS:** an item's partition is the whole signing team
//! (`teamid:<TEAM>`), so *any* binary signed by that Developer ID team, not
//! only the `aster` CLI, can read Aster root keys without a prompt. Signing a
//! binary with the team identity therefore grants it the deployment's trust
//! anchors; keep the team's signing to tools you would trust with them.
//!
//! Items also record the binary that created them as their owner. Builds
//! signed with the same Developer ID can read and
//! update each other's items without a prompt, but only the creating build
//! can delete one (`errSecInvalidOwnerEdit`), so after an upgrade deletion
//! falls back to `/usr/bin/security`. Release builds are signed with a stable
//! Developer ID and identifier (`scripts/macos/sign-cli.sh`); an ad-hoc build
//! is a different owner and may be refused or prompted. The data-protection
//! keychain would avoid this but needs an entitlement a bare CLI binary
//! cannot carry. Qualified by `scripts/macos/qualify-cli-keychain.sh`.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use zeroize::Zeroizing;

pub const STORE_ENV: &str = "ASTER_KEYSTORE";
pub const SERVICE_ENV: &str = "ASTER_KEYRING_SERVICE";
const DEFAULT_SERVICE: &str = "aster";

/// A 32-byte Ed25519 seed, wiped from memory on drop.
pub struct RootSecret(Zeroizing<[u8; 32]>);

impl RootSecret {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// A fresh key from the OS random number generator.
    pub fn generate() -> Result<Self> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::getrandom(bytes.as_mut())
            .map_err(|e| anyhow::anyhow!("OS random number generator: {e}"))?;
        Ok(Self(bytes))
    }

    /// The Ed25519 public key, as 64 lowercase hex characters. The expanded
    /// signing key is wiped when it goes out of scope.
    pub fn public_key_hex(&self) -> String {
        let key = ed25519_dalek::SigningKey::from_bytes(&self.0);
        key.verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Write the key to a new owner-only file (64 hex characters and a
    /// newline, the secret-key file format), never replacing one.
    pub fn write_new_file(&self, path: &Path) -> Result<()> {
        let mut text = self.to_hex();
        text.push('\n');
        write_secret_file(path, &text)
    }

    /// 64 hex characters, either case, surrounding whitespace ignored.
    pub fn from_hex(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(
            text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()),
            "a root key is 64 hex characters"
        );
        let mut bytes = Zeroizing::new([0u8; 32]);
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("validated hex");
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> Zeroizing<String> {
        let mut s = Zeroizing::new(String::with_capacity(64));
        for b in self.0.iter() {
            s.push(char::from_digit(u32::from(b >> 4), 16).expect("nibble"));
            s.push(char::from_digit(u32::from(b & 0xf), 16).expect("nibble"));
        }
        s
    }
}

impl fmt::Debug for RootSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RootSecret(<redacted>)")
    }
}

/// Which of a profile's keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The active root.
    Root,
    /// The committed next root, which signs the rotation to itself.
    NextRoot,
}

impl Slot {
    fn account(self, profile: &str) -> String {
        match self {
            Self::Root => format!("root-key/{profile}"),
            Self::NextRoot => format!("next-root-key/{profile}"),
        }
    }
    fn file_name(self, profile: &str) -> String {
        match self {
            Self::Root => format!("{profile}.root.key"),
            Self::NextRoot => format!("{profile}.next-root.key"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Os,
    File,
}

#[derive(Debug, Clone)]
pub struct KeyStore {
    kind: Kind,
    service: String,
    dir: PathBuf,
}

impl KeyStore {
    /// The store for this platform, or the one `ASTER_KEYSTORE` names.
    pub fn open(home: &Path) -> Result<Self> {
        let kind = match std::env::var(STORE_ENV).ok().as_deref() {
            None | Some("") => default_kind(),
            Some("os") => Kind::Os,
            Some("file") => Kind::File,
            Some(other) => bail!("{STORE_ENV} must be `os` or `file`, not `{other}`"),
        };
        if kind == Kind::Os && !cfg!(any(target_os = "macos", windows)) {
            bail!("no OS credential store support on this platform; use {STORE_ENV}=file");
        }
        let service = std::env::var(SERVICE_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVICE.into());
        Ok(Self {
            kind,
            service,
            dir: home.join("keys"),
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Where `profile`'s root key lives, for messages.
    pub fn location(&self, profile: &str) -> String {
        self.location_in(profile, Slot::Root)
    }

    /// Where `profile`'s key in `slot` lives, for messages.
    pub fn location_in(&self, profile: &str, slot: Slot) -> String {
        match self.kind {
            Kind::Os if cfg!(windows) => format!(
                "Windows Credential Manager ({}, {})",
                self.service,
                slot.account(profile)
            ),
            Kind::Os => format!(
                "macOS Keychain (service {}, account {})",
                self.service,
                slot.account(profile)
            ),
            Kind::File => self.file(profile, slot).display().to_string(),
        }
    }

    pub fn get(&self, profile: &str) -> Result<Option<RootSecret>> {
        self.get_in(profile, Slot::Root)
    }

    pub fn get_in(&self, profile: &str, slot: Slot) -> Result<Option<RootSecret>> {
        check_profile(profile)?;
        let file = self.file(profile, slot);
        let hex = match self.kind {
            Kind::Os => os::get(&self.service, &slot.account(profile))?,
            Kind::File => match std::fs::read_to_string(&file) {
                Ok(s) => {
                    let s = Zeroizing::new(s);
                    check_owner_only(&file)?;
                    Some(s)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e).with_context(|| self.location_in(profile, slot)),
            },
        };
        hex.map(|h| {
            RootSecret::from_hex(&h)
                .with_context(|| format!("reading {}", self.location_in(profile, slot)))
        })
        .transpose()
    }

    /// Store a root key for a profile that has none; an existing key is
    /// never replaced.
    pub fn put_new(&self, profile: &str, secret: &RootSecret) -> Result<()> {
        self.put_new_in(profile, Slot::Root, secret)
    }

    pub fn put_new_in(&self, profile: &str, slot: Slot, secret: &RootSecret) -> Result<()> {
        check_profile(profile)?;
        if self.get_in(profile, slot)?.is_some() {
            bail!(
                "profile `{profile}` already has a key in {}",
                self.location_in(profile, slot)
            );
        }
        match self.kind {
            Kind::Os => os::set(&self.service, &slot.account(profile), &secret.to_hex()),
            Kind::File => self.write_file(profile, slot, secret),
        }
        .with_context(|| format!("storing the key in {}", self.location_in(profile, slot)))
    }

    /// `Ok(false)` when there was no key. An error leaves the key in place.
    pub fn delete(&self, profile: &str) -> Result<bool> {
        self.delete_in(profile, Slot::Root)
    }

    pub fn delete_in(&self, profile: &str, slot: Slot) -> Result<bool> {
        check_profile(profile)?;
        match self.kind {
            Kind::Os => os::delete(&self.service, &slot.account(profile)),
            Kind::File => match std::fs::remove_file(self.file(profile, slot)) {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.into()),
            },
        }
        .with_context(|| format!("removing the key from {}", self.location_in(profile, slot)))
    }

    fn file(&self, profile: &str, slot: Slot) -> PathBuf {
        self.dir.join(slot.file_name(profile))
    }

    fn write_file(&self, profile: &str, slot: Slot, secret: &RootSecret) -> Result<()> {
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
        dir.create(&self.dir)?;
        // An existing directory keeps its mode unless tightened here.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        secret.write_new_file(&self.file(profile, slot))
    }
}

fn default_kind() -> Kind {
    if cfg!(any(target_os = "macos", windows)) {
        Kind::Os
    } else {
        Kind::File
    }
}

/// The one profile-name rule, shared by every Aster tool.
fn check_profile(profile: &str) -> Result<()> {
    aster_config::validate_profile_name(profile)
}

/// Write `text` to a new owner-only file at `path`, never replacing one: the
/// text is written and flushed to a temporary file first and then linked into
/// place, so a failure leaves no partial key behind.
pub fn write_secret_file(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut nonce = [0u8; 8];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| anyhow::anyhow!("OS random number generator: {e}"))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("key"),
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let result = (|| -> Result<()> {
        let mut file = open.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // A hard link fails if `path` exists, unlike a rename.
        match std::fs::hard_link(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("{} already exists; it is never overwritten", path.display())
            }
            Err(e) => Err(e.into()),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Refuse a key file others can read or write, as ssh does.
#[cfg(unix)]
fn check_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    ensure!(
        mode & 0o077 == 0,
        "{} is accessible to other users (mode {mode:o}); run `chmod 600` on it",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn check_owner_only(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
mod os {
    use std::sync::OnceLock;

    use anyhow::Result;
    use keyring_core::{Entry, Error};
    use zeroize::Zeroizing;

    fn init() -> Result<()> {
        static INIT: OnceLock<Result<(), String>> = OnceLock::new();
        INIT.get_or_init(|| {
            #[cfg(target_os = "macos")]
            let store = apple_native_keyring_store::keychain::Store::new();
            #[cfg(windows)]
            let store = windows_native_keyring_store::Store::new();
            store
                .map(|s| keyring_core::set_default_store(s))
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(anyhow::Error::msg)
    }

    pub fn get(service: &str, account: &str) -> Result<Option<Zeroizing<String>>> {
        init()?;
        match Entry::new(service, account)?.get_password() {
            Ok(v) => Ok(Some(Zeroizing::new(v))),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
        init()?;
        Ok(Entry::new(service, account)?.set_password(secret)?)
    }

    pub fn delete(service: &str, account: &str) -> Result<bool> {
        init()?;
        match Entry::new(service, account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(Error::NoEntry) => Ok(false),
            #[cfg(target_os = "macos")]
            Err(Error::NoStorageAccess(e)) => delete_with_security_tool(service, account)
                .map_err(|fallback| anyhow::anyhow!("{e}; `security` fallback: {fallback}")),
            Err(e) => Err(e.into()),
        }
    }

    /// An item created by an earlier build of the CLI can be read but not
    /// deleted by this one; Apple's `security` tool can. Only the service and
    /// account names reach its command line.
    #[cfg(target_os = "macos")]
    fn delete_with_security_tool(service: &str, account: &str) -> Result<bool> {
        let out = std::process::Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-s", service, "-a", account])
            .stdin(std::process::Stdio::null())
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(true)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod os {
    use anyhow::{Result, bail};
    use zeroize::Zeroizing;

    pub fn get(_: &str, _: &str) -> Result<Option<Zeroizing<String>>> {
        bail!("no OS credential store on this platform")
    }
    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        bail!("no OS credential store on this platform")
    }
    pub fn delete(_: &str, _: &str) -> Result<bool> {
        bail!("no OS credential store on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_store(tag: &str) -> (KeyStore, PathBuf) {
        let home =
            std::env::temp_dir().join(format!("aster-keystore-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let store = KeyStore {
            kind: Kind::File,
            service: "unused".into(),
            dir: home.join("keys"),
        };
        (store, home)
    }

    #[test]
    fn hex_round_trip_and_validation() {
        let s = RootSecret::from_hex(&"Ab".repeat(32)).unwrap();
        assert_eq!(s.as_bytes(), &[0xab; 32]);
        assert_eq!(s.to_hex().as_str(), "ab".repeat(32));
        assert!(RootSecret::from_hex("abc").is_err());
        assert!(RootSecret::from_hex(&"zz".repeat(32)).is_err());
        assert_eq!(format!("{s:?}"), "RootSecret(<redacted>)");
    }

    #[test]
    fn file_store_lifecycle_never_overwrites() {
        let (store, home) = file_store("lifecycle");
        assert!(store.get("dev").unwrap().is_none());
        store
            .put_new("dev", &RootSecret::from_bytes([7; 32]))
            .unwrap();
        assert_eq!(store.get("dev").unwrap().unwrap().as_bytes(), &[7; 32]);
        assert!(
            store
                .put_new("dev", &RootSecret::from_bytes([8; 32]))
                .is_err()
        );
        assert_eq!(store.get("dev").unwrap().unwrap().as_bytes(), &[7; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&home.join("keys")), 0o700);
            assert_eq!(mode(&home.join("keys/dev.root.key")), 0o600);
        }
        assert!(store.delete("dev").unwrap());
        assert!(!store.delete("dev").unwrap());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn public_key_matches_rfc8032() {
        let s = RootSecret::from_hex(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        )
        .unwrap();
        assert_eq!(
            s.public_key_hex(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
    }

    #[cfg(unix)]
    #[test]
    fn loose_permissions_are_tightened_or_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (store, home) = file_store("perms");
        std::fs::create_dir_all(home.join("keys")).unwrap();
        std::fs::set_permissions(home.join("keys"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        store
            .put_new("dev", &RootSecret::from_bytes([1; 32]))
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&home.join("keys")),
            0o700,
            "existing directory tightened"
        );
        let key = home.join("keys/dev.root.key");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            store
                .get("dev")
                .unwrap_err()
                .to_string()
                .contains("chmod 600")
        );
        let leftovers: Vec<_> = std::fs::read_dir(home.join("keys")).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary files left behind");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn profile_names_cannot_escape_the_directory() {
        let (store, home) = file_store("names");
        for bad in ["../x", "a/b", "", "a.b", "Prod"] {
            assert!(store.get(bad).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(home);
    }
}
