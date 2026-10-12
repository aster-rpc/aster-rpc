//! `aster profile list|show|delete`: representative command family.

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::json;

use crate::{Ctx, config, keystore, out};

#[derive(Subcommand, Debug)]
pub enum ProfileCommand {
    /// List profiles.
    List,
    /// Show profile details.
    Show { name: Option<String> },
    /// Delete a profile and its root key.
    Delete {
        name: String,
        /// Skip confirmation.
        #[arg(long, short)]
        yes: bool,
    },
}

pub fn run(ctx: &Ctx, cmd: ProfileCommand) -> Result<ExitCode> {
    match cmd {
        ProfileCommand::List => list(ctx),
        ProfileCommand::Show { name } => show(ctx, name),
        ProfileCommand::Delete { name, yes } => delete(ctx, &name, yes),
    }
}

pub fn list_json(ctx: &Ctx) -> Result<serde_json::Value> {
    let config = config::load(&ctx.home)?;
    let active = config.active_profile.as_deref();
    let rows: Vec<_> = config
        .profiles
        .iter()
        .map(|(name, p)| json!({"name": name, "active": Some(name.as_str()) == active, "root_pubkey": p.root_pubkey}))
        .collect();
    Ok(json!({"profiles": rows}))
}

fn list(ctx: &Ctx) -> Result<ExitCode> {
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&list_json(ctx)?)?);
        return Ok(ExitCode::SUCCESS);
    }
    let config = config::load(&ctx.home)?;
    if config.profiles.is_empty() {
        println!("No profiles configured.");
        return Ok(ExitCode::SUCCESS);
    }
    let active = config.active_profile.as_deref();
    let rows: Vec<Vec<String>> = config
        .profiles
        .iter()
        .map(|(name, p)| {
            let marker = if Some(name.as_str()) == active {
                "*"
            } else {
                ""
            };
            vec![
                marker.into(),
                name.clone(),
                p.root_pubkey.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    println!("{}", out::table(&["", "PROFILE", "ROOT PUBKEY"], &rows));
    Ok(ExitCode::SUCCESS)
}

fn show(ctx: &Ctx, name: Option<String>) -> Result<ExitCode> {
    let config = config::load(&ctx.home)?;
    let Some(name) = name.or(config.active_profile.clone()) else {
        bail!("no active profile; name one");
    };
    let Some(profile) = config.profiles.get(&name) else {
        bail!("profile '{name}' does not exist");
    };
    let key_state = match keystore::get(&keystore::root_key_user(&name)) {
        Ok(Some(_)) => "in credential store".to_string(),
        Ok(None) => "not set".to_string(),
        Err(e) => format!("unavailable ({e:#})"),
    };
    let active = config.active_profile.as_deref() == Some(name.as_str());
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "name": name, "active": active, "root_pubkey": profile.root_pubkey,
                "email": profile.email, "root_key": key_state,
            }))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    let tag = if active {
        format!(" {}", out::paint(out::ACTIVE, "(active)", false))
    } else {
        String::new()
    };
    println!("Profile: {name}{tag}");
    println!(
        "  root_pubkey: {}",
        profile.root_pubkey.as_deref().unwrap_or("-")
    );
    println!("  root_key:    {key_state}");
    if let Some(email) = &profile.email {
        println!("  email:       {email}");
    }
    Ok(ExitCode::SUCCESS)
}

/// Yes/no prompt on stderr. Refuses rather than blocks without a terminal.
fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("confirmation needs a terminal; pass --yes to proceed non-interactively");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn delete(ctx: &Ctx, name: &str, yes: bool) -> Result<ExitCode> {
    let mut config = config::load(&ctx.home)?;
    if !config.profiles.contains_key(name) {
        bail!("profile '{name}' does not exist");
    }
    if !yes && !confirm(&format!("Delete profile '{name}' and its root key?"))? {
        println!("Cancelled.");
        return Ok(ExitCode::SUCCESS);
    }
    // Remove the key first: a profile must not disappear while its root key
    // silently stays behind.
    let key_deleted = keystore::delete(&keystore::root_key_user(name))
        .map_err(|e| e.context("could not remove the root key; profile left unchanged"))?;
    config.profiles.remove(name);
    if config.active_profile.as_deref() == Some(name) {
        config.active_profile = config.profiles.keys().next().cloned();
    }
    config::save(&ctx.home, &config)?;
    if ctx.json {
        println!(
            "{}",
            json!({"deleted": name, "root_key_deleted": key_deleted})
        );
    } else {
        println!("Deleted profile '{name}'.");
    }
    Ok(ExitCode::SUCCESS)
}
