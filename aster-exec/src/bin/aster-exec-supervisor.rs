#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = std::env::args_os()
        .nth(1)
        .ok_or("expected sealed spec path")?;
    if std::env::args_os().nth(2).is_some() {
        return Err("unexpected supervisor argument".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err("sealed spec must be a root-owned private regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(aster_exec::contract::MAX_LOCAL_MESSAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > aster_exec::contract::MAX_LOCAL_MESSAGE_BYTES {
        return Err("sealed spec exceeds size limit".into());
    }
    let spec: aster_exec::linux_supervisor::SupervisorSpecV1 = serde_json::from_slice(&bytes)?;
    aster_exec::linux_supervisor::run(spec)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("aster-exec-supervisor is available only on Linux");
    std::process::exit(2);
}
