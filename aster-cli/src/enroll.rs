//! Enrollment issuance adapter; credential semantics live in Aster core.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aster::enrollment::{
    AdmissionClaims, EnrollmentIssueRequest, JwtEnrollmentIssuer, TypedAttribute,
};
use aster_cli_kit::out::{Report, Styler};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

#[derive(Debug, Subcommand)]
pub enum EnrollCommand {
    /// Issue a reusable JWT for an existing node public key; does not register it in Docs
    Node(NodeArgs),
}

#[derive(Debug, Args)]
pub struct NodeArgs {
    /// Existing node public key (64 hex characters); no node private key is needed
    #[arg(long)]
    subject: String,
    /// Intended application/fleet audience
    #[arg(long)]
    audience: String,
    /// Explicit root secret-key file; otherwise use the selected profile's keystore
    #[arg(long, value_name = "FILE")]
    root_key: Option<PathBuf>,
    /// New JWT file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
    /// Expiry as Unix seconds (exclusive); choose this or --no-expiry
    #[arg(
        long,
        required_unless_present = "no_expiry",
        conflicts_with = "no_expiry"
    )]
    expires_at: Option<u64>,
    /// Explicitly issue without expiry, relying on revocation and consumer policy
    #[arg(long)]
    no_expiry: bool,
    /// Earliest use as Unix seconds
    #[arg(long)]
    not_before: Option<u64>,
    /// Granted scope; repeat for multiple scopes
    #[arg(long)]
    scope: Vec<String>,
    /// Root-asserted classification tag; repeat for multiple tags
    #[arg(long)]
    tag: Vec<String>,
    /// JSON object of typed attributes, e.g. {"region":{"type":"string","value":"eu"}}
    #[arg(long, value_name = "FILE")]
    attributes: Option<PathBuf>,
}

#[derive(Serialize)]
struct IssuedReport {
    root: String,
    subject: String,
    audience: String,
    credential_id: String,
    expires_at: Option<u64>,
    credential_file: PathBuf,
    registered: bool,
}

impl Report for IssuedReport {
    fn human(&self, _: Styler) -> String {
        format!(
            "Issued enrollment credential for {}\nCredential: {}\nRoot: {}\nAudience: {}\nExpiry: {}\nThis issues a credential; it does not publish a Docs registration.",
            self.subject,
            self.credential_file.display(),
            self.root,
            self.audience,
            self.expires_at
                .map_or_else(|| "none (explicit)".into(), |v| v.to_string()),
        )
    }
}

pub async fn run(ctx: &Ctx, command: EnrollCommand) -> Result<()> {
    match command {
        EnrollCommand::Node(args) => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let report = issue(ctx, args, now)?;
            ctx.emit(&report)
        }
    }
}

fn issue(ctx: &Ctx, args: NodeArgs, now: u64) -> Result<IssuedReport> {
    let config = aster_config::load(&ctx.home)?;
    let profile_name = config.selected_profile_name(ctx.profile.as_deref());
    let expected_root = config
        .profiles
        .get(&profile_name)
        .and_then(|p| p.root_pubkey.as_ref())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Denied,
                "selected profile has no configured root public key",
            )
            .with_hint("configure the profile root before issuing enrollment credentials")
        })?;
    // Never accept the secret itself as a command argument or print its input.
    let secret = match &args.root_key {
        Some(path) => {
            let text = zeroize::Zeroizing::new(read_bounded(path, 128, true)?);
            aster_keystore::RootSecret::from_hex(
                std::str::from_utf8(&text)
                    .map_err(|_| Error::usage("root key file must contain a hex seed"))?,
            )
            .map_err(|_| Error::usage("root key file must contain exactly 64 hex characters"))?
        }
        None => aster_keystore::KeyStore::open(&ctx.home)?
            .get(&profile_name)?
            .ok_or_else(|| {
                Error::not_found("selected profile has no root private key")
                    .with_hint("run `aster key root generate` or supply --root-key FILE")
            })?,
    };
    let issuer = JwtEnrollmentIssuer::from_seed(secret.as_bytes());
    let root = hex::encode(issuer.public_key());
    if !expected_root.eq_ignore_ascii_case(&root) {
        return Err(Error::new(
            ErrorKind::Denied,
            "root key does not match the selected profile",
        ));
    }
    let subject: [u8; 32] = hex::decode(&args.subject)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| Error::usage("subject must be a 32-byte public key in hex"))?;
    let attributes: BTreeMap<String, TypedAttribute> = match args.attributes {
        Some(path) => {
            AdmissionClaims::parse_attributes_json(&read_bounded(&path, 16 * 1024, false)?)
                .map_err(|_| Error::usage("attributes must be a JSON object of typed values"))?
        }
        None => BTreeMap::new(),
    };
    let mut request = EnrollmentIssueRequest::new(subject, args.audience.clone());
    request.not_before = args.not_before;
    request.expires_at = args.expires_at;
    request.claims = AdmissionClaims {
        scopes: Some(args.scope.into_iter().collect::<BTreeSet<_>>()),
        tags: Some(args.tag.into_iter().collect::<BTreeSet<_>>()),
        attributes: Some(attributes),
    };
    let issued = issuer
        .issue(&request, now)
        .map_err(|e| Error::usage(format!("cannot issue enrollment credential: {e}")))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Publish only a complete file, atomically and without replacing an
    // existing path. The random credential id is generated by the issuer.
    let parent = args
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".aster-enrollment-{}.tmp", issued.credential_id()));
    let mut out = options.open(&temporary)?;
    let written = (|| -> std::io::Result<()> {
        writeln!(out, "{}", issued.jwt())?;
        out.sync_all()?;
        drop(out);
        std::fs::hard_link(&temporary, &args.out)
    })();
    let _ = std::fs::remove_file(&temporary);
    written?;
    Ok(IssuedReport {
        root,
        subject: hex::encode(subject),
        audience: args.audience,
        credential_id: issued.credential_id().into(),
        expires_at: args.expires_at,
        credential_file: args.out,
        registered: false,
    })
}

fn read_bounded(path: &Path, limit: u64, secret: bool) -> Result<Vec<u8>> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(Error::usage("enrollment inputs must be regular files"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // Recheck the opened descriptor below. Nonblocking open also prevents a
    // regular file replaced by a FIFO between these checks from hanging.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::usage("enrollment inputs must be regular files"));
    }
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            return Err(
                Error::usage("root key file must have owner-only permissions")
                    .with_hint("restrict the key file to mode 0600 before retrying"),
            );
        }
    }
    #[cfg(not(unix))]
    let _ = secret;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::usage("enrollment input exceeds its size limit"));
    }
    Ok(bytes)
}
