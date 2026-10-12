//! Root keys in the OS credential store: macOS Keychain and Windows
//! Credential Manager via keyring-core. Linux is an open decision (see the
//! CLI doc); the spike reports it as unavailable rather than pulling in a
//! Secret Service client.

use anyhow::Result;
use keyring_core::{Entry, Error};

/// Spike-only override so tests never touch real entries.
pub fn service() -> String {
    std::env::var("ASTER_KEYRING_SERVICE").unwrap_or_else(|_| "aster".to_string())
}

pub fn root_key_user(profile: &str) -> String {
    format!("root-key/{profile}")
}

fn init() -> Result<()> {
    #[cfg(target_os = "macos")]
    keyring_core::set_default_store(apple_native_keyring_store::keychain::Store::new()?);
    #[cfg(windows)]
    keyring_core::set_default_store(windows_native_keyring_store::Store::new()?);
    #[cfg(not(any(target_os = "macos", windows)))]
    anyhow::bail!("no OS credential store support on this platform yet");
    #[allow(unreachable_code)]
    Ok(())
}

pub fn get(user: &str) -> Result<Option<String>> {
    init()?;
    match Entry::new(&service(), user)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete(user: &str) -> Result<bool> {
    init()?;
    match Entry::new(&service(), user)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(Error::NoEntry) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
