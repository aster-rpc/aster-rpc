//! `aster profile …`: operator profiles in the shared `~/.aster/config.toml`
//! (`aster_config`). The reference command family for the conventions in
//! `aster_cli_kit`. Root keys arrive with the key-store port.

use std::sync::Arc;

use aster_cli_kit::out::{DIM, OK, Styler, table};
use aster_cli_kit::tool::{NoArgs, no_arguments};
use aster_cli_kit::{Ctx, Error, Report, Result, Tool, prompt};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::json;

use aster_config::{Config, Handle, HandleStatus, Profile, Signer};
use aster_keystore::KeyStore;

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// List profiles
    List,
    /// Show a profile (the active one by default)
    Show {
        /// Profile name
        name: Option<String>,
    },
    /// Create a profile; the first one becomes active
    Create {
        /// Profile name: 1-64 characters from a-z, 0-9, `-` and `_`
        name: String,
    },
    /// Make a profile the active one
    Use {
        /// Profile name
        name: String,
    },
    /// Delete a profile
    Delete {
        /// Profile name
        name: String,
        /// Do not ask for confirmation
        #[arg(long, short)]
        yes: bool,
    },
}

pub async fn run(ctx: &Ctx, cmd: ProfileCommand) -> Result<()> {
    match cmd {
        ProfileCommand::List => ctx.emit(&list(ctx)?),
        ProfileCommand::Show { name } => ctx.emit(&show(ctx, name.as_deref())?),
        ProfileCommand::Create { name } => ctx.emit(&create(ctx, &name)?),
        ProfileCommand::Use { name } => ctx.emit(&activate(ctx, &name)?),
        ProfileCommand::Delete { name, yes } => {
            known(&aster_config::load(&ctx.home)?, &name)?;
            let store = KeyStore::open(&ctx.home)?;
            let question = match store.get(&name)? {
                Some(_) => format!(
                    "Delete profile `{name}` and its root private key from {}? Without a backup this cannot be undone.",
                    store.location(&name)
                ),
                None => format!("Delete profile `{name}`?"),
            };
            if !yes && !prompt::confirm(ctx, &question)? {
                return Err(Error::new(
                    aster_cli_kit::ErrorKind::Cancelled,
                    "Not deleted.",
                ));
            }
            ctx.emit(&delete(ctx, &name)?)
        }
    }
}

pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "profile_list",
            "List Aster operator profiles and which one is active.",
            no_arguments(),
            |ctx: Arc<Ctx>, _: NoArgs| async move { list(&ctx) },
        )
        .read_only(),
        Tool::new(
            "profile_show",
            "Show one Aster operator profile, the active one if no name is given.",
            json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Profile name"}},
                "additionalProperties": false,
            }),
            |ctx: Arc<Ctx>, args: ShowArgs| async move { show(&ctx, args.name.as_deref()) },
        )
        .read_only(),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowArgs {
    name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProfileView {
    name: String,
    active: bool,
    root_pubkey: Option<String>,
    email: Option<String>,
    signer: Signer,
    published_services: Vec<String>,
    handle: Option<Handle>,
}

#[derive(Debug, Serialize)]
pub struct ProfileList {
    profiles: Vec<ProfileView>,
}

impl Report for ProfileList {
    fn human(&self, style: Styler) -> String {
        if self.profiles.is_empty() {
            return "No profiles. Create one with `aster profile create <name>`.".into();
        }
        let rows: Vec<Vec<String>> = self
            .profiles
            .iter()
            .map(|p| {
                vec![
                    if p.active { "*".into() } else { String::new() },
                    p.name.clone(),
                    p.root_pubkey.clone().unwrap_or_else(|| "-".into()),
                ]
            })
            .collect();
        table(style, &["", "PROFILE", "ROOT PUBKEY"], &rows)
    }
}

impl Report for ProfileView {
    fn human(&self, style: Styler) -> String {
        let none = || style.paint(DIM, "none");
        let active = if self.active {
            style.paint(OK, " (active)")
        } else {
            String::new()
        };
        let handle = match &self.handle {
            None => none(),
            Some(h) if h.status == HandleStatus::Verified => format!("@{}", h.name),
            Some(h) => format!("@{} (pending verification)", h.name),
        };
        let services = match self.published_services.as_slice() {
            [] => none(),
            s => s.join(", "),
        };
        let rows = [
            ("root pubkey", self.root_pubkey.clone().unwrap_or_else(none)),
            ("handle", handle),
            ("email", self.email.clone().unwrap_or_else(none)),
            ("signer", signer_name(self.signer).into()),
            ("published", services),
        ];
        let mut out = format!("{}{active}", self.name);
        for (k, v) in rows {
            out.push_str(&format!(
                "\n  {}  {v}",
                style.paint(DIM, &format!("{k:<11}"))
            ));
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileAction {
    Created,
    Activated,
    Deleted,
}

#[derive(Debug, Serialize)]
pub struct Changed {
    profile: String,
    action: ProfileAction,
    active: bool,
}

impl Report for Changed {
    fn human(&self, style: Styler) -> String {
        let (verb, note) = match (self.action, self.active) {
            (ProfileAction::Created, true) => ("Created", " It is now active."),
            (ProfileAction::Created, false) => ("Created", ""),
            (ProfileAction::Activated, _) => ("Activated", ""),
            (ProfileAction::Deleted, _) => ("Deleted", ""),
        };
        format!(
            "{} profile `{}`.{note}",
            style.paint(OK, verb),
            self.profile
        )
    }
}

fn view(cfg: &Config, name: &str, p: &Profile) -> ProfileView {
    ProfileView {
        name: name.to_string(),
        active: cfg.active_profile == name,
        root_pubkey: p.root_pubkey.clone(),
        email: p.email.clone(),
        signer: p.signer,
        published_services: p.published_services.clone(),
        handle: p.handle.clone(),
    }
}

fn signer_name(signer: Signer) -> &'static str {
    match signer {
        Signer::Local => "local credential store",
    }
}

fn known<'a>(cfg: &'a Config, name: &str) -> Result<&'a Profile> {
    cfg.profiles.get(name).ok_or_else(|| {
        Error::not_found(format!("no profile `{name}`"))
            .with_hint("list profiles with `aster profile list`")
    })
}

fn list(ctx: &Ctx) -> Result<ProfileList> {
    let cfg = aster_config::load(&ctx.home)?;
    Ok(ProfileList {
        profiles: cfg.profiles.iter().map(|(n, p)| view(&cfg, n, p)).collect(),
    })
}

/// `name`, else `--profile` / `ASTER_PROFILE`, else the active profile.
fn show(ctx: &Ctx, name: Option<&str>) -> Result<ProfileView> {
    let cfg = aster_config::load(&ctx.home)?;
    let name = cfg.selected_profile_name(name.or(ctx.profile.as_deref()));
    let profile = known(&cfg, &name).map_err(|e| {
        e.with_hint("name a profile, or create one with `aster profile create <name>`")
    })?;
    Ok(view(&cfg, &name, profile))
}

/// The shared rule (`aster_config::validate_profile_name`), as a usage error.
fn validate_name(name: &str) -> Result<()> {
    aster_config::validate_profile_name(name).map_err(|e| Error::usage(e.to_string()))
}

fn create(ctx: &Ctx, name: &str) -> Result<Changed> {
    validate_name(name)?;
    let active = aster_config::update(&ctx.home, |cfg| -> Result<bool> {
        if cfg.profiles.contains_key(name) {
            return Err(Error::usage(format!("profile `{name}` already exists")));
        }
        cfg.profiles.insert(name.to_string(), Profile::default());
        // Become active when the active name points at nothing.
        let active = !cfg.profiles.contains_key(&cfg.active_profile);
        if active {
            cfg.active_profile = name.to_string();
        }
        Ok(active)
    })?;
    Ok(Changed {
        profile: name.into(),
        action: ProfileAction::Created,
        active,
    })
}

fn activate(ctx: &Ctx, name: &str) -> Result<Changed> {
    aster_config::update(&ctx.home, |cfg| -> Result<()> {
        known(cfg, name)?;
        cfg.active_profile = name.to_string();
        Ok(())
    })?;
    Ok(Changed {
        profile: name.into(),
        action: ProfileAction::Activated,
        active: true,
    })
}

/// The root key goes first: if the store refuses (a macOS Keychain item owned
/// by another binary, for example), the profile is left unchanged.
fn delete(ctx: &Ctx, name: &str) -> Result<Changed> {
    KeyStore::open(&ctx.home)?.delete(name).map_err(|e| {
        Error::from(e.context("could not remove the root key; profile left unchanged"))
    })?;
    let was_active = aster_config::update(&ctx.home, |cfg| -> Result<bool> {
        known(cfg, name)?;
        cfg.profiles.remove(name);
        Ok(cfg.active_profile == name)
    })?;
    Ok(Changed {
        profile: name.into(),
        action: ProfileAction::Deleted,
        active: was_active,
    })
}
