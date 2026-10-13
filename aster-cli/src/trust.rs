//! `aster trust …`: root pre-rotation (ownership-attestations §7).
//!
//! A profile pins its root `R0` and a commitment to the next root `R1`
//! (`next_root_commitment`). Only `R1` can sign the statement that replaces
//! `R0`, so a compromised `R0` cannot rotate. Keep `R1` apart from `R0`:
//! after `aster trust init`, export it to a file you move offline
//! (`aster trust next export`) and delete it here (`aster trust next
//! delete`); import it again only to rotate.
//!
//! Rotating (`aster trust rotate --issue`) signs the statement with `R1`,
//! makes `R1` this profile's root and commits to a fresh `R2`. Other
//! profiles and nodes apply the statement file (`aster trust rotate
//! --apply`); enrolled nodes also learn it from any replica during the
//! enrollment handshake. Records and credentials from the old root confer
//! nothing afterwards: export the reviewed active set before rotating
//! (`aster enroll export`) and republish it under the new root (`aster
//! enroll republish`).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aster::delegation::{
    DEFAULT_LEEWAY_SECONDS, MAX_ROTATION_BYTES, RootState, RotationReason, issue_root_rotation,
    next_root_commitment, verify_root_rotation,
};
use aster_cli_kit::out::{DIM, OK, Report, Styler};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result, prompt};
use aster_keystore::{KeyStore, RootSecret, Slot};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use crate::files::read_bounded;
use crate::key::{RootKeyAction, parse_key, profile_name, store_root};

#[derive(Debug, Subcommand)]
pub enum TrustCommand {
    /// Commit the profile to a next root key, generating the root too if it has none
    Init,
    /// Show the profile's root, next-root commitment and rotation count
    Show,
    /// The committed next root key, kept apart from the root
    #[command(subcommand)]
    Next(NextCommand),
    /// Rotate to the committed next root, or apply a rotation statement
    Rotate(RotateArgs),
}

#[derive(Debug, Subcommand)]
pub enum NextCommand {
    /// Write the next root private key to a new owner-only file, to keep offline
    Export {
        /// File to create; an existing file is never overwritten
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
    },
    /// Store the next root private key again, checked against the commitment
    Import {
        /// File holding the key as 64 hex characters, or `-` for standard input
        #[arg(value_name = "FILE")]
        from: PathBuf,
    },
    /// Delete the next root private key from this machine's store
    Delete {
        /// Do not ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("mode").required(true).args(["issue", "apply"])))]
pub struct RotateArgs {
    /// Sign the rotation with the committed next root and make it this profile's root
    #[arg(long, requires_all = ["fleet_namespace", "out"])]
    issue: bool,
    /// Apply a rotation statement file to this profile's pinned root
    #[arg(long, value_name = "FILE")]
    apply: Option<PathBuf>,
    /// The new root's fleet namespace (64 lowercase hex characters)
    #[arg(long, requires = "issue")]
    fleet_namespace: Option<String>,
    /// Why the root rotates
    #[arg(long, value_enum, default_value_t = Reason::Routine, requires = "issue")]
    reason: Reason,
    /// Statement file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE", requires = "issue")]
    out: Option<PathBuf>,
    /// Delete the previous root's private key from this machine once rotated
    #[arg(long, requires = "issue")]
    delete_previous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Reason {
    Routine,
    Compromise,
}

pub async fn run(ctx: &Ctx, command: TrustCommand) -> Result<()> {
    match command {
        TrustCommand::Init => ctx.emit(&init(ctx)?),
        TrustCommand::Show => ctx.emit(&show(ctx)?),
        TrustCommand::Next(NextCommand::Export { out }) => ctx.emit(&export_next(ctx, &out)?),
        TrustCommand::Next(NextCommand::Import { from }) => {
            import_next(ctx, &from)?;
            ctx.emit(&show(ctx)?)
        }
        TrustCommand::Next(NextCommand::Delete { yes }) => {
            let name = profile_name(ctx)?;
            let store = KeyStore::open(&ctx.home)?;
            if store.get_in(&name, Slot::NextRoot)?.is_none() {
                return Err(Error::not_found(format!(
                    "profile `{name}` has no next root private key in {}",
                    store.location_in(&name, Slot::NextRoot)
                )));
            }
            let question = format!(
                "Delete the next root private key of profile `{name}` from {}? Without the exported copy the root can never rotate.",
                store.location_in(&name, Slot::NextRoot)
            );
            if !yes && !prompt::confirm(ctx, &question)? {
                return Err(Error::new(ErrorKind::Cancelled, "Not deleted."));
            }
            store.delete_in(&name, Slot::NextRoot)?;
            ctx.emit(&show(ctx)?)
        }
        TrustCommand::Rotate(args) => match args.apply {
            Some(file) => ctx.emit(&apply(ctx, &file)?),
            None => ctx.emit(&issue(ctx, &args)?),
        },
    }
}

/// What a profile pins: its root, the next-root commitment and the count of
/// rotations that led here.
#[derive(Debug, Serialize)]
pub struct TrustView {
    profile: String,
    root_pubkey: Option<String>,
    next_root_commitment: Option<String>,
    root_seq: u64,
    root_key_stored: bool,
    next_root_key_stored: bool,
    next_root_key_store: String,
}

impl Report for TrustView {
    fn human(&self, style: Styler) -> String {
        let none = || style.paint(DIM, "none");
        let stored = |yes: bool, at: &str| {
            if yes {
                at.to_owned()
            } else {
                style.paint(DIM, "not stored here")
            }
        };
        format!(
            "profile `{}`\n  {}  {}\n  {}  {}\n  {}  {}\n  {}  {}",
            self.profile,
            style.paint(DIM, "root            "),
            self.root_pubkey.clone().unwrap_or_else(none),
            style.paint(DIM, "next commitment "),
            self.next_root_commitment.clone().unwrap_or_else(none),
            style.paint(DIM, "rotations       "),
            self.root_seq,
            style.paint(DIM, "next root key   "),
            stored(self.next_root_key_stored, &self.next_root_key_store),
        )
    }
}

fn show(ctx: &Ctx) -> Result<TrustView> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let cfg = aster_config::load(&ctx.home)?;
    let profile = cfg.profiles.get(&name).cloned().unwrap_or_default();
    Ok(TrustView {
        root_key_stored: store.get(&name)?.is_some(),
        next_root_key_stored: store.get_in(&name, Slot::NextRoot)?.is_some(),
        next_root_key_store: store.location_in(&name, Slot::NextRoot),
        root_pubkey: profile.root_pubkey,
        next_root_commitment: profile.next_root_commitment,
        root_seq: profile.root_seq,
        profile: name,
    })
}

#[derive(Debug, Serialize)]
pub struct Initialized {
    #[serde(flatten)]
    view: TrustView,
    generated_root: bool,
}

impl Report for Initialized {
    fn human(&self, style: Styler) -> String {
        format!(
            "{} profile `{}` to a next root.\n{}\nNow move the next root offline: `aster trust next export --out <FILE>`, then `aster trust next delete`. Pin `next_root_commitment` wherever the root is pinned (node profiles, Exec configuration).",
            style.paint(OK, "Committed"),
            self.view.profile,
            self.view.human(style),
        )
    }
}

/// Generate `R1` into the next-root slot and pin its commitment. Generates
/// `R0` first when the profile has no root at all.
fn init(ctx: &Ctx) -> Result<Initialized> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let profile = aster_config::load(&ctx.home)?
        .profiles
        .get(&name)
        .cloned()
        .unwrap_or_default();
    if let Some(commitment) = &profile.next_root_commitment {
        return Err(Error::usage(format!(
            "profile `{name}` is already committed to a next root ({commitment})"
        ))
        .with_hint("rotate with `aster trust rotate --issue`; a commitment is never replaced"));
    }
    let generated_root = profile.root_pubkey.is_none() && store.get(&name)?.is_none();
    if generated_root {
        store_root(ctx, &RootSecret::generate()?, RootKeyAction::Generated)?;
    }
    let next = RootSecret::generate()?;
    store.put_new_in(&name, Slot::NextRoot, &next)?;
    let commitment = hex::encode(next_root_commitment(&public(&next)));
    let recorded = aster_config::update(&ctx.home, |cfg| -> Result<()> {
        let profile = cfg.profiles.entry(name.clone()).or_default();
        profile.next_root_commitment = Some(commitment.clone());
        Ok(())
    });
    if let Err(error) = recorded {
        let _ = store.delete_in(&name, Slot::NextRoot);
        return Err(error);
    }
    Ok(Initialized {
        view: show(ctx)?,
        generated_root,
    })
}

#[derive(Debug, Serialize)]
pub struct NextExported {
    profile: String,
    file: PathBuf,
}

impl Report for NextExported {
    fn human(&self, style: Styler) -> String {
        format!(
            "{} the next root private key of profile `{}` to {}.\nKeep it offline and apart from the root; it alone can rotate the root. Then run `aster trust next delete`.",
            style.paint(OK, "Wrote"),
            self.profile,
            self.file.display()
        )
    }
}

fn export_next(ctx: &Ctx, out: &Path) -> Result<NextExported> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let secret = store.get_in(&name, Slot::NextRoot)?.ok_or_else(|| {
        Error::not_found(format!("profile `{name}` has no next root private key"))
    })?;
    secret
        .write_new_file(out)
        .map_err(|e| Error::from(e).with_kind(ErrorKind::Usage))?;
    Ok(NextExported {
        profile: name,
        file: out.to_path_buf(),
    })
}

fn import_next(ctx: &Ctx, from: &Path) -> Result<()> {
    let name = profile_name(ctx)?;
    let text = if from == Path::new("-") {
        let mut text = zeroize::Zeroizing::new(String::new());
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        zeroize::Zeroizing::new(
            String::from_utf8(read_bounded(from, 4096, true)?)
                .map_err(|_| Error::usage("the key file must be text"))?,
        )
    };
    let secret = parse_key(&text)?;
    let profile = aster_config::load(&ctx.home)?
        .profiles
        .get(&name)
        .cloned()
        .unwrap_or_default();
    let commitment = profile.next_root_commitment.ok_or_else(|| {
        Error::usage(format!("profile `{name}` is not committed to a next root"))
            .with_hint("run `aster trust init` first")
    })?;
    if hex::encode(next_root_commitment(&public(&secret))) != commitment {
        return Err(Error::new(
            ErrorKind::Denied,
            "this key is not the committed next root of the profile",
        ));
    }
    KeyStore::open(&ctx.home)?.put_new_in(&name, Slot::NextRoot, &secret)?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Rotated {
    profile: String,
    action: &'static str,
    previous_root: String,
    root_pubkey: String,
    root_seq: u64,
    next_root_commitment: String,
    fleet_namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    statement_file: Option<PathBuf>,
}

impl Report for Rotated {
    fn human(&self, style: Styler) -> String {
        let mut out = format!(
            "{} profile `{}` from root {} to {} (rotation {}).\n  fleet namespace  {}\n  next commitment  {}",
            style.paint(OK, self.action),
            self.profile,
            self.previous_root,
            self.root_pubkey,
            self.root_seq,
            self.fleet_namespace,
            self.next_root_commitment,
        );
        if let Some(file) = &self.statement_file {
            out.push_str(&format!(
                "\nStatement: {}\nNext: republish the reviewed records (`aster enroll republish`), issue new delegations and credentials, give the statement to nodes and replicas (`aster trust rotate --apply`), and move the new next root offline (`aster trust next export`).",
                file.display()
            ));
        }
        out
    }
}

/// Sign `R0 -> R1` with the committed `R1`, commit to a fresh `R2`, and make
/// `R1` this profile's root.
fn issue(ctx: &Ctx, args: &RotateArgs) -> Result<Rotated> {
    let name = profile_name(ctx)?;
    let store = KeyStore::open(&ctx.home)?;
    let profile = aster_config::load(&ctx.home)?
        .profiles
        .get(&name)
        .cloned()
        .unwrap_or_default();
    let state = pinned(&name, &profile)?;
    let next = store.get_in(&name, Slot::NextRoot)?.ok_or_else(|| {
        Error::not_found(format!(
            "profile `{name}` has no next root private key on this machine"
        ))
        .with_hint("import the offline copy with `aster trust next import <FILE>`")
    })?;
    if next_root_commitment(&public(&next)) != state.next_commitment {
        return Err(Error::new(
            ErrorKind::Denied,
            "the stored next root key does not match the profile's commitment",
        ));
    }
    let fleet = hex32(
        args.fleet_namespace.as_deref().unwrap_or_default(),
        "fleet namespace",
    )?;
    let out = args.out.as_ref().expect("clap requires --out with --issue");
    // The previous root confers nothing once rotated; its key leaves the
    // root slot, so make the operator say so before anything is written.
    let holds_previous = store.get(&name)?.is_some();
    if holds_previous && !args.delete_previous {
        return Err(Error::usage(format!(
            "profile `{name}` holds the current root's private key, which the new root replaces"
        ))
        .with_hint(
            "export it first if you need it (`aster key root export`), then pass --delete-previous",
        ));
    }
    let following = RootSecret::generate()?;
    let following_commitment = next_root_commitment(&public(&following));
    let statement = issue_root_rotation(
        &aster::SecretKey::from_bytes(*next.as_bytes()),
        &aster::PublicKey::from_bytes(state.root),
        state.seq + 1,
        &following_commitment,
        &[fleet],
        unix_now()?,
        match args.reason {
            Reason::Routine => RotationReason::Routine,
            Reason::Compromise => RotationReason::Compromise,
        },
    )
    .map_err(|e| Error::new(ErrorKind::Failure, format!("signing the rotation: {e:?}")))?;
    let rotation = verify_root_rotation(&statement, &state, unix_now()?, DEFAULT_LEEWAY_SECONDS)
        .map_err(|e| Error::new(ErrorKind::Failure, format!("rotation self-check: {e:?}")))?;
    aster_keystore::write_secret_file(out, &format!("{statement}\n"))
        .map_err(|e| Error::from(e).with_kind(ErrorKind::Usage))?;
    // The statement is durable; now move the keys. The committed key leaves
    // the next slot only once it holds the root slot.
    if holds_previous {
        store.delete(&name)?;
    }
    store.put_new(&name, &next)?;
    store.delete_in(&name, Slot::NextRoot)?;
    store.put_new_in(&name, Slot::NextRoot, &following)?;
    record(ctx, &name, &rotation, "Rotated", Some(out.clone()))
}

/// Apply a statement to this profile's pinned root.
fn apply(ctx: &Ctx, file: &Path) -> Result<Rotated> {
    let name = profile_name(ctx)?;
    let profile = aster_config::load(&ctx.home)?
        .profiles
        .get(&name)
        .cloned()
        .unwrap_or_default();
    let state = pinned(&name, &profile)?;
    let statement = String::from_utf8(read_bounded(file, MAX_ROTATION_BYTES + 2, false)?)
        .map_err(|_| Error::usage("a rotation statement is text"))?;
    let rotation = verify_root_rotation(
        statement.trim(),
        &state,
        unix_now()?,
        DEFAULT_LEEWAY_SECONDS,
    )
    .map_err(|e| {
        Error::new(
            ErrorKind::Denied,
            format!("the statement does not rotate this profile's root: {e:?}"),
        )
        .with_hint("it must be signed by the committed next root, name the current root, and be the next rotation")
    })?;
    record(ctx, &name, &rotation, "Applied a rotation to", None)
}

fn pinned(name: &str, profile: &aster_config::Profile) -> Result<RootState> {
    let root = profile
        .root_pubkey
        .as_deref()
        .ok_or_else(|| Error::usage(format!("profile `{name}` has no root public key")))?;
    let commitment = profile.next_root_commitment.as_deref().ok_or_else(|| {
        Error::usage(format!("profile `{name}` is not committed to a next root"))
            .with_hint("the root cannot rotate without a commitment; see `aster trust init`")
    })?;
    Ok(RootState {
        root: hex32(root, "profile root public key")?,
        next_commitment: hex32(commitment, "next root commitment")?,
        seq: profile.root_seq,
    })
}

fn record(
    ctx: &Ctx,
    name: &str,
    rotation: &aster::delegation::RootRotation,
    action: &'static str,
    statement_file: Option<PathBuf>,
) -> Result<Rotated> {
    let fleet = hex::encode(rotation.namespaces()[0]);
    let state = rotation.apply();
    aster_config::update(&ctx.home, |cfg| -> Result<()> {
        let profile = cfg.profiles.entry(name.to_owned()).or_default();
        profile.root_pubkey = Some(hex::encode(state.root));
        profile.next_root_commitment = Some(hex::encode(state.next_commitment));
        profile.root_seq = state.seq;
        profile.exec_policy_namespace = Some(fleet.clone());
        // Kept so the node relays the rotation to peers still on the old
        // root; the enrollment service verifies the history against the pin.
        let statement = rotation.statement().to_owned();
        if !profile.root_rotations.contains(&statement) {
            profile.root_rotations.push(statement);
        }
        let excess = profile
            .root_rotations
            .len()
            .saturating_sub(aster::enrollment::MAX_FORWARDED_ROTATIONS);
        profile.root_rotations.drain(..excess);
        Ok(())
    })?;
    Ok(Rotated {
        profile: name.to_owned(),
        action,
        previous_root: hex::encode(rotation.previous_root()),
        root_pubkey: hex::encode(state.root),
        root_seq: state.seq,
        next_root_commitment: hex::encode(state.next_commitment),
        fleet_namespace: fleet,
        statement_file,
    })
}

fn hex32(value: &str, what: &str) -> Result<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .filter(|bytes| hex::encode(bytes) == value)
        .ok_or_else(|| Error::usage(format!("{what} must be 64 lowercase hex characters")))
}

fn public(secret: &RootSecret) -> [u8; 32] {
    aster::SecretKey::from_bytes(*secret.as_bytes())
        .public_key()
        .to_bytes()
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
