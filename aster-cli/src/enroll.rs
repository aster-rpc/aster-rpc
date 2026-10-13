//! Enrollment issuance adapter; credential semantics live in Aster core.
//!
//! Credentials are delegation chains. The root signs one-link credentials and
//! delegations to online issuers; an issuer signs credentials under its own
//! delegation chain. Every link expires, and lifetimes are sized to the
//! profile's root cadence (enrollment design §4).

mod policy;
mod publish;

use crate::files::read_bounded;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use aster::delegation::{
    Chain, DelegationGrant, LinkKind, MAX_CHAIN_BYTES, MAX_LINK_LIFETIME, Validity,
    issue_delegation,
};
use aster::enrollment::{
    AdmissionClaims, Audience, EnrollmentIssueRequest, JwtEnrollmentIssuer, JwtEnrollmentVerifier,
    Registration, TypedAttribute,
};
use aster_cli_kit::out::{Report, Styler};
use aster_cli_kit::{Ctx, Error, ErrorKind, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

const DAY: u64 = 86_400;
/// Default lifetime of an ephemeral credential, and of a delegated persistent
/// one (renewed through its issuer).
const DEFAULT_LEAF_LIFETIME: u64 = DAY;
/// Floor for root-renewed links when the profile declares no root cadence.
const UNSET_CADENCE_MINIMUM: u64 = 7 * DAY;

#[derive(Debug, Subcommand)]
pub enum EnrollCommand {
    /// Issue a credential for an existing node public key; does not register it in Docs
    Node(NodeArgs),
    /// Delegate bounded issuing authority to an online issuer, such as an orchestrator
    Delegate(DelegateArgs),
    /// Create policy namespaces and bind them to the fleet as the root
    #[command(subcommand)]
    Namespace(policy::NamespaceCommand),
    /// Revoke or reactivate an online issuer as the root
    Issuer(policy::IssuerArgs),
}

#[derive(Debug, Args)]
pub struct SignerArgs {
    /// Explicit root secret-key file; otherwise use the selected profile's keystore
    #[arg(long, value_name = "FILE", conflicts_with = "signer")]
    root_key: Option<PathBuf>,
    /// Sign as a delegated issuer with this secret-key file (needs --parent)
    #[arg(long, value_name = "FILE", requires = "parent")]
    signer: Option<PathBuf>,
    /// The delegated issuer's own chain from the root (needs --signer)
    #[arg(long, value_name = "FILE", requires = "signer")]
    parent: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct LifetimeArgs {
    /// Expiry as Unix seconds (exclusive)
    #[arg(long, conflicts_with = "expires_in")]
    expires_at: Option<u64>,
    /// Lifetime from now, such as "24h" or "7d"; the default follows the
    /// profile's root cadence
    #[arg(long, value_name = "DURATION")]
    expires_in: Option<String>,
    /// Allow a lifetime shorter than the profile's root cadence requires
    #[arg(long)]
    allow_short_lifetime: bool,
}

#[derive(Debug, Args)]
pub struct NodeArgs {
    /// Existing node public key (64 hex characters); no node private key is needed
    #[arg(long)]
    subject: String,
    /// Intended application/fleet audience
    #[arg(long)]
    audience: String,
    #[command(flatten)]
    signer: SignerArgs,
    /// New credential file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
    #[command(flatten)]
    lifetime: LifetimeArgs,
    /// The node expects lasting membership: the root promotes it to a
    /// registration record. Otherwise the credential is ephemeral.
    #[arg(long)]
    persistent: bool,
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

#[derive(Debug, Args)]
pub struct DelegateArgs {
    /// The issuer's public key (64 hex characters)
    #[arg(long)]
    subject: String,
    /// Fleet audience; the delegate's credentials carry it
    #[arg(long)]
    audience: String,
    /// A further audience the delegate's credentials may carry; repeatable
    #[arg(long)]
    child_audience: Vec<String>,
    #[command(flatten)]
    signer: SignerArgs,
    /// New delegation chain file to create; an existing file is never overwritten
    #[arg(long, value_name = "FILE")]
    out: PathBuf,
    #[command(flatten)]
    lifetime: LifetimeArgs,
    /// Longest lifetime of a credential the delegate signs, such as "24h"
    #[arg(long, value_name = "DURATION")]
    max_child_lifetime: String,
    /// Scope the delegate may grant; repeatable
    #[arg(long)]
    scope: Vec<String>,
    /// Tag the delegate may assert; repeatable
    #[arg(long)]
    tag: Vec<String>,
    /// Attribute key the delegate may assert; repeatable
    #[arg(long)]
    attribute_key: Vec<String>,
    /// Let the delegate enroll persistent nodes
    #[arg(long)]
    may_register: bool,
    /// Further delegation levels allowed below the delegate (0: leaves only)
    #[arg(long, default_value_t = 0)]
    max_depth: u32,
}

#[derive(Serialize)]
struct IssuedReport {
    kind: &'static str,
    root: String,
    subject: String,
    audience: String,
    credential_id: Option<String>,
    issuers: Vec<String>,
    registration: Option<Registration>,
    expires_at: u64,
    credential_file: PathBuf,
    registered: bool,
}

impl Report for IssuedReport {
    fn human(&self, _: Styler) -> String {
        let what = match self.kind {
            "delegation" => "delegation chain",
            _ => "enrollment credential",
        };
        let registration = match self.registration {
            Some(Registration::Persistent) => {
                "\nRegistration: persistent (the root promotes it to a record)"
            }
            Some(Registration::Ephemeral) => "\nRegistration: ephemeral (never promoted)",
            None => "",
        };
        format!(
            "Issued {what} for {}\nFile: {}\nRoot: {}\nAudience: {}\nExpiry: {}{registration}\nThis issues a credential; it does not publish a Docs registration.",
            self.subject,
            self.credential_file.display(),
            self.root,
            self.audience,
            self.expires_at,
        )
    }
}

pub async fn run(ctx: &Ctx, command: EnrollCommand) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    match command {
        EnrollCommand::Node(args) => ctx.emit(&issue_node(ctx, args, now)?),
        EnrollCommand::Delegate(args) => ctx.emit(&issue_delegate(ctx, args, now)?),
        EnrollCommand::Namespace(command) => policy::namespace(ctx, command).await,
        EnrollCommand::Issuer(args) => policy::issuer(ctx, args, now).await,
    }
}

/// The profile's pinned root and declared cadence.
struct ProfileTrust {
    name: String,
    root: [u8; 32],
    cadence: Option<u64>,
    replication: u64,
}

fn profile_trust(ctx: &Ctx) -> Result<ProfileTrust> {
    let config = aster_config::load(&ctx.home)?;
    let name = config.selected_profile_name(ctx.profile.as_deref());
    let profile = config.profiles.get(&name);
    let root = profile
        .and_then(|p| p.root_pubkey.as_ref())
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Denied,
                "selected profile has no configured root public key",
            )
            .with_hint("configure the profile root before issuing enrollment credentials")
        })?;
    let root = parse_key(root, "profile root public key")?;
    let cadence = profile
        .and_then(|p| p.root_cadence.as_deref())
        .map(parse_duration)
        .transpose()?;
    let replication = profile
        .and_then(|p| p.replication_allowance.as_deref())
        .map(parse_duration)
        .transpose()?
        .unwrap_or(0);
    Ok(ProfileTrust {
        name,
        root,
        cadence,
        replication,
    })
}

/// The secret that signs, and the chain it signs under (none for the root).
struct Signer {
    secret: zeroize::Zeroizing<[u8; 32]>,
    parent: Option<Chain>,
}

fn signer(ctx: &Ctx, trust: &ProfileTrust, args: &SignerArgs) -> Result<Signer> {
    if let (Some(key), Some(parent)) = (&args.signer, &args.parent) {
        let secret = read_secret(key)?;
        let text = String::from_utf8(read_bounded(parent, MAX_CHAIN_BYTES + 1, false)?)
            .map_err(|_| Error::usage("parent chain file must be text"))?;
        let parent = Chain::from_text(text.trim())
            .map_err(|e| Error::usage(format!("parent chain file is not a chain: {e}")))?;
        return Ok(Signer {
            secret,
            parent: Some(parent),
        });
    }
    // Never accept the secret itself as a command argument or print its input.
    let secret = match &args.root_key {
        Some(path) => read_secret(path)?,
        None => {
            let stored = aster_keystore::KeyStore::open(&ctx.home)?
                .get(&trust.name)?
                .ok_or_else(|| {
                    Error::not_found("selected profile has no root private key")
                        .with_hint("run `aster key root generate` or supply --root-key FILE")
                })?;
            zeroize::Zeroizing::new(*stored.as_bytes())
        }
    };
    if JwtEnrollmentIssuer::from_seed(&secret).public_key() != trust.root {
        return Err(Error::new(
            ErrorKind::Denied,
            "root key does not match the selected profile",
        ));
    }
    Ok(Signer {
        secret,
        parent: None,
    })
}

fn read_secret(path: &Path) -> Result<zeroize::Zeroizing<[u8; 32]>> {
    let text = zeroize::Zeroizing::new(read_bounded(path, 128, true)?);
    let secret = aster_keystore::RootSecret::from_hex(
        std::str::from_utf8(&text).map_err(|_| Error::usage("key file must contain a hex seed"))?,
    )
    .map_err(|_| Error::usage("key file must contain exactly 64 hex characters"))?;
    Ok(zeroize::Zeroizing::new(*secret.as_bytes()))
}

/// Pick the expiry and enforce the sizing rule for links the root renews:
/// `lifetime >= 2C + R` (margin M = C), or 7 days when C is unset.
fn expiry(args: &LifetimeArgs, now: u64, root_renewed: bool, trust: &ProfileTrust) -> Result<u64> {
    let minimum = root_renewed.then(|| match trust.cadence {
        Some(cadence) => cadence.saturating_mul(2).saturating_add(trust.replication),
        None => UNSET_CADENCE_MINIMUM,
    });
    let lifetime = match (args.expires_at, &args.expires_in) {
        (Some(at), _) => at.saturating_sub(now),
        (None, Some(duration)) => parse_duration(duration)?,
        (None, None) => minimum.unwrap_or(DEFAULT_LEAF_LIFETIME),
    };
    if lifetime == 0 {
        return Err(Error::usage("the credential would already be expired"));
    }
    if lifetime > MAX_LINK_LIFETIME {
        return Err(Error::usage(format!(
            "lifetime exceeds the {} day maximum for any link",
            MAX_LINK_LIFETIME / DAY
        )));
    }
    if let Some(minimum) =
        minimum.filter(|minimum| lifetime < *minimum && !args.allow_short_lifetime)
    {
        let basis = match trust.cadence {
            Some(_) => "twice the profile's root_cadence plus replication_allowance",
            None => "7 days, because the profile declares no root_cadence",
        };
        return Err(Error::usage(format!(
            "lifetime {lifetime}s is shorter than {minimum}s ({basis}); the root may not renew it in time"
        ))
        .with_hint("lengthen the lifetime, set root_cadence, or pass --allow-short-lifetime"));
    }
    Ok(now + lifetime)
}

fn issue_node(ctx: &Ctx, args: NodeArgs, now: u64) -> Result<IssuedReport> {
    let trust = profile_trust(ctx)?;
    let signer = signer(ctx, &trust, &args.signer)?;
    let subject = parse_key(&args.subject, "subject")?;
    let registration = if args.persistent {
        Registration::Persistent
    } else {
        Registration::Ephemeral
    };
    // Only a persistent, root-issued credential waits for the root to
    // promote it; an issuer renews its own leaves.
    let root_renewed = args.persistent && signer.parent.is_none();
    let expires_at = expiry(&args.lifetime, now, root_renewed, &trust)?;
    let attributes: BTreeMap<String, TypedAttribute> = match args.attributes {
        Some(path) => {
            AdmissionClaims::parse_attributes_json(&read_bounded(&path, 16 * 1024, false)?)
                .map_err(|_| Error::usage("attributes must be a JSON object of typed values"))?
        }
        None => BTreeMap::new(),
    };
    let mut request = EnrollmentIssueRequest::new(subject, args.audience.clone(), expires_at);
    request.not_before = args.not_before;
    request.registration = registration;
    request.claims = AdmissionClaims {
        scopes: Some(args.scope.into_iter().collect::<BTreeSet<_>>()),
        tags: Some(args.tag.into_iter().collect::<BTreeSet<_>>()),
        attributes: Some(attributes),
    };
    let issuer = JwtEnrollmentIssuer::from_seed(&signer.secret);
    let issued = match &signer.parent {
        None => issuer.issue(&request, now),
        Some(parent) => issuer.issue_delegated(&request, parent, now),
    }
    .map_err(|e| Error::usage(format!("cannot issue enrollment credential: {e}")))?;
    // The result must verify against the profile's pinned root, whoever signed.
    let evidence = JwtEnrollmentVerifier::new(trust.root, args.audience.clone())
        .and_then(|verifier| {
            verifier.verify(
                issued.chain(),
                &subject,
                now.max(args.not_before.unwrap_or(now)),
            )
        })
        .map_err(|e| {
            Error::new(
                ErrorKind::Denied,
                format!("issued credential does not verify against the profile root: {e}"),
            )
        })?;
    let issuers = match evidence.source() {
        aster::enrollment::AdmissionEvidenceSource::PresentedChain { issuers, .. } => {
            issuers.iter().map(hex::encode).collect()
        }
        _ => vec![],
    };
    write_new(&args.out, issued.credential_id(), issued.chain())?;
    Ok(IssuedReport {
        kind: "enrollment",
        root: hex::encode(trust.root),
        subject: hex::encode(subject),
        audience: args.audience,
        credential_id: Some(issued.credential_id().into()),
        issuers,
        registration: Some(registration),
        expires_at,
        credential_file: args.out,
        registered: false,
    })
}

fn issue_delegate(ctx: &Ctx, args: DelegateArgs, now: u64) -> Result<IssuedReport> {
    let trust = profile_trust(ctx)?;
    let signer = signer(ctx, &trust, &args.signer)?;
    let subject = parse_key(&args.subject, "subject")?;
    // Only the root renews a delegation, so it must outlive missed windows.
    let expires_at = expiry(&args.lifetime, now, true, &trust)?;
    let mut audiences: BTreeSet<String> = args.child_audience.into_iter().collect();
    audiences.insert(args.audience.clone());
    let grant = DelegationGrant {
        may_issue: if args.max_depth > 0 {
            [LinkKind::Delegation, LinkKind::Enrollment].into()
        } else {
            [LinkKind::Enrollment].into()
        },
        audiences,
        scopes: args.scope.into_iter().collect(),
        tags: args.tag.into_iter().collect(),
        attribute_keys: args.attribute_key.into_iter().collect(),
        max_child_lifetime: parse_duration(&args.max_child_lifetime)?,
        max_depth: args.max_depth,
        subjects: None,
        operations: BTreeSet::new(),
        mandatory: BTreeSet::new(),
        may_register: args.may_register,
    };
    let chain = issue_delegation(
        &aster::SecretKey::from_bytes(*signer.secret),
        signer.parent.as_ref(),
        &aster::PublicKey::from_bytes(subject),
        &Audience::One(args.audience.clone()),
        &grant,
        Validity {
            issued_at: now,
            not_before: None,
            expires_at,
        },
    )
    .map_err(|e| Error::usage(format!("cannot issue delegation: {e}")))?;
    // The signer of the new link; a sub-delegation's ancestors are in its file.
    let issuers = vec![hex::encode(
        JwtEnrollmentIssuer::from_seed(&signer.secret).public_key(),
    )];
    write_new(&args.out, &hex::encode(chain.digest()), chain.to_text())?;
    Ok(IssuedReport {
        kind: "delegation",
        root: hex::encode(trust.root),
        subject: hex::encode(subject),
        audience: args.audience,
        credential_id: None,
        issuers,
        registration: None,
        expires_at,
        credential_file: args.out,
        registered: false,
    })
}

/// Publish only a complete file, atomically and without replacing an
/// existing path.
pub(crate) fn write_new(out: &Path, unique: &str, text: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".aster-enrollment-{unique}.tmp"));
    let mut file = options.open(&temporary)?;
    let written = (|| -> std::io::Result<()> {
        writeln!(file, "{text}")?;
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(&temporary, out)
    })();
    let _ = std::fs::remove_file(&temporary);
    written?;
    Ok(())
}

pub(crate) fn parse_key(value: &str, what: &str) -> Result<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| Error::usage(format!("{what} must be a 32-byte public key in hex")))
}

/// `<n>s`, `<n>m`, `<n>h` or `<n>d`; a bare number is seconds.
pub(crate) fn parse_duration(value: &str) -> Result<u64> {
    let value = value.trim();
    let (digits, unit) = match value.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (&value[..index], unit),
        _ => (value, 's'),
    };
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => DAY,
        _ => 0,
    };
    digits
        .parse::<u64>()
        .ok()
        .filter(|_| scale > 0)
        .and_then(|n| n.checked_mul(scale))
        .ok_or_else(|| {
            Error::usage(format!(
                "invalid duration `{value}`; use a number with s, m, h or d"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn durations_are_strict() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("6h").unwrap(), 21_600);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        for invalid in ["", "h", "6x", "-1h", "1.5h", "99999999999999999999d"] {
            assert!(parse_duration(invalid).is_err(), "{invalid}");
        }
    }
}
