//! Idempotent owner preflight: validates existing files; never invokes sudo or
//! installs/starts a service implicitly.
#[cfg(target_os = "linux")]
use super::grants::{bounded, identity};
use aster_cli_kit::{Error, ErrorKind, Result};
use clap::Args;
#[cfg(target_os = "linux")]
use serde::Deserialize;
#[cfg(target_os = "linux")]
use serde_json::{Value, json};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Debug, Args)]
pub struct SetupArgs {
    /// Existing root-owned server JSON configuration
    #[arg(long, default_value = "/etc/aster-exec/server.json")]
    server_config: PathBuf,
    /// Existing root-owned privileged helper JSON configuration
    #[arg(long, default_value = "/etc/aster-exec/helper.json")]
    helper_config: PathBuf,
    /// Explicit unprivileged API service UID
    #[arg(long)]
    api_uid: u32,
    /// Explicit dedicated payload UID, distinct from API and root
    #[arg(long)]
    payload_uid: u32,
    #[arg(long, default_value = "/usr/local/libexec/aster-exec-server")]
    server_bin: PathBuf,
    #[arg(long, default_value = "/usr/local/libexec/aster-exec-helper")]
    helper_bin: PathBuf,
    #[arg(long, default_value = "/usr/local/libexec/aster-exec-supervisor")]
    supervisor_bin: PathBuf,
}
#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct ServerConfig {
    state_directory: PathBuf,
    root_public_key: String,
    policy_namespace: String,
    audience: String,
    policy_read_ticket_file: PathBuf,
    credential_file: PathBuf,
    helper_socket: PathBuf,
    ceiling: aster_exec::authorization::LocalCeilingV1,
    resource_limits: aster_exec::contract::ResourceLimitsV1,
    audit: Value,
    reconcile_interval_ms: u64,
}
#[cfg(target_os = "linux")]
#[derive(Deserialize)]
struct HelperConfig {
    socket: PathBuf,
    state_root: PathBuf,
    supervisor: PathBuf,
    systemd_run: PathBuf,
    api_uid: u32,
    api_gid: u32,
    payload_uid: u32,
    payload_gid: u32,
    payload_groups: Vec<u32>,
    resource_limits: aster_exec::contract::ResourceLimitsV1,
}
#[cfg(target_os = "linux")]
fn owner_file(path: &Path, expected_uid: u32, secret: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.uid() != expected_uid
        || metadata.mode() & if secret { 0o077 } else { 0o027 } != 0
    {
        return Err(Error::new(
            ErrorKind::Denied,
            format!(
                "{} must be a regular file owned by UID {expected_uid}, mode {} or stricter",
                path.display(),
                if secret { "0600" } else { "0640" }
            ),
        ));
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute() {
        return Err(Error::usage("installed executable paths must be absolute"));
    }
    for component in path.ancestors() {
        let meta = std::fs::symlink_metadata(component)?;
        if meta.file_type().is_symlink() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
            return Err(Error::new(
                ErrorKind::Denied,
                format!(
                    "{} and executable ancestors must be root-owned, nonsymlink and not writable by group/others",
                    path.display()
                ),
            ));
        }
    }
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.mode() & 0o111 == 0 {
        return Err(Error::usage(format!(
            "{} is not an executable file",
            path.display()
        )));
    }
    Ok(())
}
#[cfg(any(target_os = "linux", test))]
fn validate_unit_paths(
    socket: &std::path::Path,
    state_root: &std::path::Path,
    api_state: &std::path::Path,
) -> Result<()> {
    if socket.parent() != Some(std::path::Path::new("/run/aster-exec"))
        || state_root != std::path::Path::new("/var/lib/aster-exec-helper")
        || api_state != std::path::Path::new("/var/lib/aster-exec-api")
    {
        return Err(Error::usage(
            "paths must match the shipped systemd units: helper socket within /run/aster-exec, helper state_root /var/lib/aster-exec-helper, API state_directory /var/lib/aster-exec-api; customize and review the units separately for other layouts",
        ));
    }
    Ok(())
}
#[cfg(target_os = "linux")]
pub fn run(args: SetupArgs) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::new(
            ErrorKind::Denied,
            "setup preflight must run as the machine owner (root) to inspect installed credentials; this command does not invoke sudo",
        ));
    }
    if args.api_uid == 0 || args.payload_uid == 0 || args.api_uid == args.payload_uid {
        return Err(Error::usage(
            "API and payload UIDs must be distinct non-root accounts",
        ));
    }
    owner_file(&args.server_config, 0, false)?;
    owner_file(&args.helper_config, 0, true)?;
    let server: ServerConfig =
        serde_json::from_slice(&bounded(&args.server_config, 65536, false)?)?;
    let helper: HelperConfig = serde_json::from_slice(&bounded(&args.helper_config, 65536, true)?)?;
    validate_unit_paths(&helper.socket, &helper.state_root, &server.state_directory)?;
    if helper.api_gid == 0
        || helper.payload_gid == 0
        || helper.api_gid == helper.payload_gid
        || helper.payload_groups.contains(&0)
        || helper.payload_groups.contains(&helper.api_gid)
    {
        return Err(Error::usage(
            "API and payload GIDs must be distinct nonzero groups; payload supplementary groups must exclude root and the API group",
        ));
    }
    if helper.api_uid != args.api_uid
        || helper.payload_uid != args.payload_uid
        || helper.socket != server.helper_socket
        || helper.supervisor != args.supervisor_bin
        || helper.resource_limits != server.resource_limits
    {
        return Err(Error::usage(
            "helper accounts, socket, supervisor path and resource limits must match the explicit setup choices and server configuration",
        ));
    }
    for path in [
        &args.server_bin,
        &args.helper_bin,
        &args.supervisor_bin,
        &helper.systemd_run,
    ] {
        executable(path)?;
    }
    for path in [
        &server.state_directory,
        &helper.state_root,
        &server.helper_socket,
    ] {
        if !path.is_absolute() {
            return Err(Error::usage(
                "state and control socket paths must be absolute",
            ));
        }
    }
    if !(200..=2000).contains(&server.reconcile_interval_ms)
        || server.ceiling.version != 1
        || server.ceiling.default_profile != "open"
        || server.ceiling.enabled_profiles != ["open".to_string()].into()
        || server.ceiling.cwd_roots.is_empty()
        || server
            .ceiling
            .cwd_roots
            .iter()
            .any(|p| !p.is_absolute() || !p.is_dir())
        || server.ceiling.max_timeout_ms == 0
        || server.ceiling.max_timeout_ms > aster_exec::contract::MAX_TIMEOUT_MS
        || server.ceiling.max_output_bytes == 0
        || server.ceiling.max_output_bytes > aster_exec::contract::MAX_OUTPUT_BYTES
    {
        return Err(Error::usage(
            "invalid Linux open-profile ceiling, working directories, limits or reconciliation interval",
        ));
    }
    let audit = server
        .audit
        .get("directory")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::usage("audit.directory is required"))?;
    let audit = Path::new(audit);
    let escapes = audit
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir));
    if escapes || !audit.starts_with(&server.state_directory) {
        return Err(Error::usage(
            "audit directory must be within the API state directory",
        ));
    }
    let key = server.state_directory.join("node.key");
    owner_file(&key, args.api_uid, true)?;
    let state = std::fs::symlink_metadata(&server.state_directory)?;
    if !state.is_dir() || state.uid() != args.api_uid || state.mode() & 0o077 != 0 {
        return Err(Error::new(
            ErrorKind::Denied,
            "API state directory must be owned by its UID with mode 0700",
        ));
    }
    let node_secret = aster::SecretKey::read_file(&key)?;
    let target = aster::attestation::public_key(&node_secret).to_hex();
    if target != server.ceiling.target_id {
        return Err(Error::usage(
            "persistent node key does not match the ceiling target ID",
        ));
    }
    owner_file(&server.credential_file, 0, false)?;
    owner_file(&server.policy_read_ticket_file, 0, false)?;
    let credential = String::from_utf8(bounded(&server.credential_file, 65536, false)?)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::usage("invalid system clock"))?
        .as_secs();
    aster::enrollment::JwtEnrollmentVerifier::new(
        identity(&server.root_public_key)?,
        &server.audience,
    )?
    .verify(credential.trim(), &identity(&target)?, now)?;
    let ticket_text = String::from_utf8(bounded(&server.policy_read_ticket_file, 65536, false)?)?;
    let ticket: aster::native::iroh_docs::DocTicket = ticket_text.trim().parse()?;
    if ticket.capability.id().as_bytes() != &identity(&server.policy_namespace)?
        || !matches!(
            ticket.capability,
            aster::native::iroh_docs::Capability::Read(_)
        )
    {
        return Err(Error::usage(
            "policy ticket must be read-only and match the pinned namespace",
        ));
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file()
        || !Path::new("/run/systemd/system").is_dir()
    {
        return Err(Error::new(
            ErrorKind::Unavailable,
            "Linux unified cgroup v2 and a running systemd are required",
        ));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"configuration_preflight_passed":true,"target_id":target,"api_uid":args.api_uid,"payload_uid":args.payload_uid,"exec_ready":false,"next_steps":["Install the reviewed root-owned systemd units with the configured accounts and executable paths","Start aster-exec-helper.service and aster-exec-server.service using the machine owner's service manager","Publish the exact-root Exec grant and sync policy, then run aster exec check and a diagnostic job"],"note":"No files changed and no services started. Preflight does not prove remote grant acceptance, helper reachability, audit delivery or payload termination."})
        )?
    );
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn run(_: SetupArgs) -> Result<()> {
    Err(Error::new(
        ErrorKind::Unavailable,
        "Exec setup preflight supports Linux systemd/cgroup v2 targets only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn preflight_paths_match_systemd_write_permissions() {
        let socket = Path::new("/run/aster-exec/helper.sock");
        let helper = Path::new("/var/lib/aster-exec-helper");
        let api = Path::new("/var/lib/aster-exec-api");
        assert!(validate_unit_paths(socket, helper, api).is_ok());
        assert!(validate_unit_paths(Path::new("/tmp/helper.sock"), helper, api).is_err());
        assert!(validate_unit_paths(socket, Path::new("/tmp/helper"), api).is_err());
        assert!(validate_unit_paths(socket, helper, Path::new("/tmp/api")).is_err());
    }
}
