//! `aster key …`: profile root keys (the offline trust anchor of a
//! deployment) and node identity keys. Private root keys live in
//! `aster_keystore`; the profile records only the public key.
//!
//! A profile's root public key is never replaced here: once set, only the
//! matching private key can be imported, and generating needs a new profile.
//! The one way a profile's root changes is a pre-committed rotation
//! (`aster trust rotate`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aster::SecretKey;
use aster_cli_kit::out::{DIM, OK, Styler};
use aster_cli_kit::tool::{NoArgs, no_arguments};
use aster_cli_kit::{Ctx, Error, ErrorKind, Report, Result, Tool, prompt};
use aster_config::Profile;
use aster_keystore::{KeyStore, RootSecret};
use clap::Subcommand;
use serde::Serialize;

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Root keys: the trust anchor that signs a profile's credentials
    #[command(subcommand)]
    Root(RootCommand),
    /// Node identity keys
    #[command(subcommand)]
    Node(NodeCommand),
}

#[derive(Debug, Subcommand)]
pub enum RootCommand {
    /// Generate a root key for the profile, creating the profile if needed
    Generate,
    /// Store an existing root key for the profile, creating the profile if needed
    Import {
        /// File holding the key as 64 hex characters (or JSON with
        /// `private_key`), or `-` for standard input
        #[arg(value_name = "FILE")]
        from: PathBuf,
    },
    /// Write the profile's root private key to a new owner-only file
    Export {
        /// File to create; an existing file is never overwritten
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Show the profile's root public key and where its private key is kept
    Show,
    /// Delete the profile's root private key, keeping its public key
    Delete {
        /// Do not ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Generate a node identity key file and print its node ID
    Generate {
        /// File to create; an existing file is never overwritten
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
}

pub async fn run(ctx: &Ctx, cmd: KeyCommand) -> Result<()> {
    match cmd {
        KeyCommand::Root(RootCommand::Generate) => ctx.emit(&generate(ctx)?),
        KeyCommand::Root(RootCommand::Import { from }) => ctx.emit(&import(ctx, &from)?),
        KeyCommand::Root(RootCommand::Export { out }) => ctx.emit(&export(ctx, &out)?),
        KeyCommand::Root(RootCommand::Show) => ctx.emit(&show(ctx)?),
        KeyCommand::Root(RootCommand::Delete { yes }) => {
            let (name, store) = (profile_name(ctx)?, KeyStore::open(&ctx.home)?);
            if store.get(&name)?.is_none() {
                return Err(not_stored(&name, &store));
            }
            let question = format!(
                "Delete the root private key of profile `{name}` from {}? Without a backup this cannot be undone.",
                store.location(&name)
            );
            if !yes && !prompt::confirm(ctx, &question)? {
                return Err(Error::new(ErrorKind::Cancelled, "Not deleted."));
            }
            store.delete(&name)?;
            ctx.emit(&show(ctx)?)
        }
        KeyCommand::Node(NodeCommand::Generate { out }) => ctx.emit(&node_generate(&out)?),
    }
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "key_root_show",
            "Show the selected profile's root public key and whether its private key is stored.",
            no_arguments(),
            |ctx: Arc<Ctx>, _: NoArgs| async move { show(&ctx) },
        )
        .read_only(),
    ]
}

#[derive(Debug, Serialize)]
pub struct RootKeyView {
    profile: String,
    root_pubkey: Option<String>,
    /// Whether the private key is in the store.
    stored: bool,
    store: String,
}

impl Report for RootKeyView {
    fn human(&self, style: Styler) -> String {
        let key = self
            .root_pubkey
            .clone()
            .unwrap_or_else(|| style.paint(DIM, "none"));
        let private = if self.stored {
            self.store.clone()
        } else {
            style.paint(DIM, "not stored")
        };
        format!(
            "profile `{}`\n  {}  {key}\n  {}  {private}",
            self.profile,
            style.paint(DIM, "public key "),
            style.paint(DIM, "private key")
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKeyAction {
    Generated,
    Imported,
}

#[derive(Debug, Serialize)]
pub struct RootKeyChanged {
    profile: String,
    action: RootKeyAction,
    root_pubkey: String,
    store: String,
}

impl Report for RootKeyChanged {
    fn human(&self, style: Styler) -> String {
        let mut out = format!(
            "{} the root key of profile `{}`.\n  public key   {}\n  stored in    {}",
            style.paint(
                OK,
                match self.action {
                    RootKeyAction::Generated => "Generated",
                    RootKeyAction::Imported => "Imported",
                }
            ),
            self.profile,
            self.root_pubkey,
            self.store
        );
        if self.action == RootKeyAction::Generated {
            out.push_str("\nBack it up now: `aster key root export --out <FILE>`, kept offline.");
        }
        out
    }
}

#[derive(Debug, Serialize)]
pub struct Exported {
    profile: String,
    root_pubkey: String,
    file: PathBuf,
}

impl Report for Exported {
    fn human(&self, style: Styler) -> String {
        format!(
            "{} the root private key of profile `{}` to {}.\nKeep this file offline; anyone holding it can sign as this deployment.",
            style.paint(OK, "Wrote"),
            self.profile,
            self.file.display()
        )
    }
}

#[derive(Debug, Serialize)]
pub struct NodeKey {
    node_id: String,
    file: PathBuf,
}

impl Report for NodeKey {
    fn human(&self, style: Styler) -> String {
        format!(
            "{} {}\n  node id  {}\nUse it as a node identity with ASTER_SECRET_KEY_FILE={}",
            style.paint(OK, "Wrote"),
            self.file.display(),
            self.node_id,
            self.file.display()
        )
    }
}

/// `--profile` / `ASTER_PROFILE`, else the active profile.
pub(crate) fn profile_name(ctx: &Ctx) -> Result<String> {
    aster_config::load(&ctx.home)?
        .selected_profile(ctx.profile.as_deref())
        .map_err(|e| Error::from(e).with_kind(ErrorKind::Usage))
}

fn public_hex(secret: &RootSecret) -> String {
    secret.public_key_hex()
}

fn not_stored(name: &str, store: &KeyStore) -> Error {
    Error::not_found(format!(
        "profile `{name}` has no root private key in {}",
        store.location(name)
    ))
    .with_hint("generate one with `aster key root generate`, or import a backup with `aster key root import <FILE>`")
}

fn show(ctx: &Ctx) -> Result<RootKeyView> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let cfg = aster_config::load(&ctx.home)?;
    let stored = store.get(&name)?;
    let recorded = cfg.profiles.get(&name).and_then(|p| p.root_pubkey.clone());
    if let (Some(secret), Some(recorded)) = (&stored, &recorded)
        && public_hex(secret) != *recorded
    {
        return Err(Error::new(
            ErrorKind::Failure,
            format!(
                "the stored root key of profile `{name}` does not match its public key {recorded}"
            ),
        )
        .with_hint("the profile or the credential store was changed outside Aster"));
    }
    Ok(RootKeyView {
        root_pubkey: recorded.or_else(|| stored.as_ref().map(public_hex)),
        stored: stored.is_some(),
        store: store.location(&name),
        profile: name,
    })
}

fn generate(ctx: &Ctx) -> Result<RootKeyChanged> {
    let secret = RootSecret::generate()?;
    store_root(ctx, &secret, RootKeyAction::Generated)
}

fn import(ctx: &Ctx, from: &Path) -> Result<RootKeyChanged> {
    let mut text = zeroize::Zeroizing::new(String::new());
    if from == Path::new("-") {
        std::io::stdin().read_to_string(&mut text)?;
    } else {
        text = zeroize::Zeroizing::new(std::fs::read_to_string(from).map_err(|e| {
            let kind = match e.kind() {
                std::io::ErrorKind::NotFound => ErrorKind::NotFound,
                std::io::ErrorKind::PermissionDenied => ErrorKind::Denied,
                _ => ErrorKind::Failure,
            };
            Error::from(anyhow::Error::from(e).context(format!("reading {}", from.display())))
                .with_kind(kind)
        })?);
    }
    let secret = parse_key(&text)?;
    store_root(ctx, &secret, RootKeyAction::Imported)
}

/// 64 hex characters, or a JSON object with `private_key` (the Python CLI's
/// export format).
pub(crate) fn parse_key(text: &str) -> Result<RootSecret> {
    let trimmed = text.trim();
    let hex = if trimmed.starts_with('{') {
        let mut file: KeyFile = serde_json::from_str(trimmed)
            .map_err(|_| Error::usage("the key file is not valid JSON"))?;
        zeroize::Zeroizing::new(
            file.private_key
                .take()
                .ok_or_else(|| Error::usage("the key file has no `private_key`"))?,
        )
    } else {
        zeroize::Zeroizing::new(trimmed.to_string())
    };
    RootSecret::from_hex(&hex).map_err(|e| Error::usage(e.to_string()))
}

/// Only the field we need, wiped on drop (other fields are skipped unread).
#[derive(serde::Deserialize)]
struct KeyFile {
    private_key: Option<String>,
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Some(k) = &mut self.private_key {
            k.zeroize();
        }
    }
}

/// Store `secret` for the selected profile and record its public key. The
/// profile is created when missing (and made active if the active name
/// points at nothing). If recording fails, the stored key is removed again.
pub(crate) fn store_root(
    ctx: &Ctx,
    secret: &RootSecret,
    action: RootKeyAction,
) -> Result<RootKeyChanged> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let pubkey = public_hex(secret);
    let cfg = aster_config::load(&ctx.home)?;
    let recorded = cfg.profiles.get(&name).and_then(|p| p.root_pubkey.clone());
    match (&recorded, store.get(&name)?) {
        (_, Some(existing)) if public_hex(&existing) == pubkey => {
            return Err(Error::usage(format!(
                "profile `{name}` already holds this root key"
            )));
        }
        (_, Some(_)) => {
            return Err(Error::usage(format!(
                "profile `{name}` already has a different root key in {}",
                store.location(&name)
            ))
            .with_hint("use a new profile: `aster key root generate --profile <new>`"));
        }
        (Some(recorded), None) if *recorded != pubkey => {
            return Err(Error::usage(format!(
                "profile `{name}` belongs to root public key {recorded}, not {pubkey}"
            ))
            .with_hint(match action {
                RootKeyAction::Generated => {
                    "import that key's backup, or use a new profile with --profile"
                }
                RootKeyAction::Imported => {
                    "import the matching key, or use a new profile with --profile"
                }
            }));
        }
        _ => {}
    }

    store.put_new(&name, secret)?;
    let recorded = aster_config::update(&ctx.home, |cfg| -> Result<()> {
        let profile = cfg
            .profiles
            .entry(name.clone())
            .or_insert_with(Profile::default);
        profile.root_pubkey = Some(pubkey.clone());
        if !cfg.profiles.contains_key(&cfg.active_profile) {
            cfg.active_profile = name.clone();
        }
        Ok(())
    });
    if let Err(e) = recorded {
        let _ = store.delete(&name);
        return Err(e);
    }
    Ok(RootKeyChanged {
        store: store.location(&name),
        profile: name,
        action,
        root_pubkey: pubkey,
    })
}

fn export(ctx: &Ctx, out: &Path) -> Result<Exported> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let secret = store.get(&name)?.ok_or_else(|| not_stored(&name, &store))?;
    // The same file format as a node secret key: 64 hex characters, 0600,
    // never overwriting.
    secret
        .write_new_file(out)
        .map_err(|e| Error::from(e).with_kind(ErrorKind::Usage))?;
    Ok(Exported {
        root_pubkey: public_hex(&secret),
        profile: name,
        file: out.to_path_buf(),
    })
}

fn node_generate(out: &Path) -> Result<NodeKey> {
    let key = SecretKey::generate();
    key.write_file(out)
        .map_err(|e| Error::usage(e.to_string()))?;
    Ok(NodeKey {
        node_id: key.public_key().to_hex(),
        file: out.to_path_buf(),
    })
}
