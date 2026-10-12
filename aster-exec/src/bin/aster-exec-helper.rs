#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut args = std::env::args_os().skip(1);
    let path = args.next().ok_or("expected root-owned configuration")?;
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != 0 || m.mode() & 0o077 != 0 {
        return Err("helper configuration must be root-owned private file".into());
    }
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err("configuration too large".into());
    }
    let config: aster_exec::linux_helper::HelperConfig = serde_json::from_slice(&bytes)?;
    config.serve()?;
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("aster-exec-helper is Linux-only");
    std::process::exit(2);
}
