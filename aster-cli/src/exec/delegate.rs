//! Issue on-behalf-of chains: a principal delegates bounded action issuance
//! to an orchestrator, and a principal or its delegate lets an actor submit
//! Exec jobs under the principal's grant. Verification happens on the target.

use crate::enroll::{parse_duration, parse_key, write_new};
use crate::files::read_bounded;
use aster::delegation::{
    ActionGrant, Audience, Chain, DelegationGrant, LinkKind, MAX_CHAIN_BYTES, MAX_LINK_LIFETIME,
    Validity, issue_action, issue_delegation,
};
use aster_cli_kit::{Ctx, Error, Result};
use clap::Args;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default lifetime of an action leaf: long enough to start a job.
const DEFAULT_ACTION_LIFETIME: u64 = 3_600;
const DEFAULT_DELEGATION_LIFETIME: u64 = 86_400;

#[derive(Debug, Args)]
pub struct ChainSigner {
    /// Secret-key file that signs; defaults to the selected profile's node key
    #[arg(long, value_name = "FILE")]
    signer_key: Option<PathBuf>,
    /// The signer's own chain from the principal; omit when the signer is
    /// the principal
    #[arg(long, value_name = "FILE")]
    parent: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DelegateArgs {
    /// The delegate's node public key (64 hex characters), such as an orchestrator
    #[arg(long)]
    subject: String,
    /// Audience of the target's Exec grants
    #[arg(long)]
    audience: String,
    #[command(flatten)]
    signer: ChainSigner,
    /// Operation the delegate may pass on; repeatable
    #[arg(long = "operation", default_value = "exec.submit")]
    operations: Vec<String>,
    /// Longest lifetime of an action the delegate signs, such as "1h"
    #[arg(long, value_name = "DURATION")]
    max_child_lifetime: String,
    /// Further delegation levels allowed below the delegate (0: actions only)
    #[arg(long, default_value_t = 0)]
    max_depth: u32,
    /// Lifetime from now, such as "24h"
    #[arg(long, value_name = "DURATION")]
    expires_in: Option<String>,
    /// New chain file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
}

#[derive(Debug, Args)]
pub struct AuthorizeArgs {
    /// The actor's node public key (64 hex characters); it submits the job
    #[arg(long)]
    subject: String,
    /// Audience of the target's Exec grants
    #[arg(long)]
    audience: String,
    #[command(flatten)]
    signer: ChainSigner,
    /// Allowed operation; repeatable
    #[arg(long = "operation", default_value = "exec.submit")]
    operations: Vec<String>,
    /// Constraint the target must satisfy, such as `exec:target=<hex>`,
    /// `exec:profile=<name>` or `exec:cwd_root=<path>`; repeatable
    #[arg(long = "resource")]
    resources: Vec<String>,
    /// Bind the action to one request (from `aster exec run --print-request-digest`)
    #[arg(long, value_name = "HEX")]
    request_digest: Option<String>,
    /// Lifetime from now, such as "1h"
    #[arg(long, value_name = "DURATION")]
    expires_in: Option<String>,
    /// New chain file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
}

/// Read a chain file written by these commands or `aster enroll`.
pub(super) fn read_chain(path: &Path) -> Result<Chain> {
    let text = String::from_utf8(read_bounded(path, MAX_CHAIN_BYTES + 1, false)?)
        .map_err(|_| Error::usage("chain file must be text"))?;
    Chain::from_text(text.trim())
        .map_err(|e| Error::usage(format!("{} is not a chain: {e}", path.display())))
}

fn signer(ctx: &Ctx, args: &ChainSigner) -> Result<(aster::SecretKey, Option<Chain>)> {
    let key = match &args.signer_key {
        Some(path) => super::daemon::load_node_key(path)?,
        None => {
            let profile =
                aster_config::load(&ctx.home)?.selected_profile_name(ctx.profile.as_deref());
            aster_config::validate_profile_name(&profile)?;
            super::daemon::load_node_key(&super::daemon::node_key_path(ctx, &profile))?
        }
    };
    let parent = args.parent.as_deref().map(read_chain).transpose()?;
    Ok((key, parent))
}

fn validity(expires_in: Option<&str>, default: u64) -> Result<Validity> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let lifetime = expires_in
        .map(parse_duration)
        .transpose()?
        .unwrap_or(default);
    if lifetime == 0 || lifetime > MAX_LINK_LIFETIME {
        return Err(Error::usage(format!(
            "lifetime must be between 1s and {MAX_LINK_LIFETIME}s"
        )));
    }
    Ok(Validity {
        issued_at: now,
        not_before: None,
        expires_at: now + lifetime,
    })
}

fn emit(kind: &str, key: &aster::SecretKey, chain: &Chain, out: &Path) -> Result<()> {
    write_new(out, &hex::encode(chain.digest()), chain.to_text())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "kind": kind,
            "signer": key.public_key().to_hex(),
            "links": chain.links().count(),
            "chain_digest": hex::encode(chain.digest()),
            "file": out,
        }))?
    );
    Ok(())
}

pub fn delegate(ctx: &Ctx, args: DelegateArgs) -> Result<()> {
    let (key, parent) = signer(ctx, &args.signer)?;
    let subject = parse_key(&args.subject, "subject")?;
    let may_issue = if args.max_depth > 0 {
        [LinkKind::Delegation, LinkKind::Action].into()
    } else {
        [LinkKind::Action].into()
    };
    let chain = issue_delegation(
        &key,
        parent.as_ref(),
        &aster::PublicKey::from_bytes(subject),
        &Audience::One(args.audience.clone()),
        &DelegationGrant {
            may_issue,
            audiences: [args.audience].into(),
            scopes: Default::default(),
            tags: Default::default(),
            attribute_keys: Default::default(),
            max_child_lifetime: parse_duration(&args.max_child_lifetime)?,
            max_depth: args.max_depth,
            subjects: None,
            operations: args.operations.into_iter().collect(),
            mandatory: Default::default(),
            may_register: false,
        },
        validity(args.expires_in.as_deref(), DEFAULT_DELEGATION_LIFETIME)?,
    )
    .map_err(|e| Error::usage(format!("cannot issue delegation: {e}")))?;
    emit("delegation", &key, &chain, &args.out)
}

pub fn authorize(ctx: &Ctx, args: AuthorizeArgs) -> Result<()> {
    let (key, parent) = signer(ctx, &args.signer)?;
    let subject = parse_key(&args.subject, "subject")?;
    if let Some(digest) = &args.request_digest
        && (digest.len() != 64
            || !digest
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    {
        return Err(Error::usage(
            "request digest must be 64 lowercase hex characters",
        ));
    }
    let chain = issue_action(
        &key,
        parent.as_ref(),
        &aster::PublicKey::from_bytes(subject),
        &Audience::One(args.audience),
        &ActionGrant {
            operations: args.operations.into_iter().collect(),
            resources: args.resources.into_iter().collect(),
            request: args.request_digest,
            mandatory: Default::default(),
        },
        validity(args.expires_in.as_deref(), DEFAULT_ACTION_LIFETIME)?,
    )
    .map_err(|e| Error::usage(format!("cannot issue action: {e}")))?;
    emit("action", &key, &chain, &args.out)
}
