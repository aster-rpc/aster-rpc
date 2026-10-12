//! Generate the command reference: `cargo run -p aster-cli --example docgen -- <dir>`.
//! Writes `<dir>/aster.md` and one man page per command to `<dir>/man/`.

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "target/docs".into()),
    );
    let cmd = aster_cli::command();
    std::fs::create_dir_all(dir.join("man"))?;
    std::fs::write(dir.join("aster.md"), aster_cli_kit::docs::markdown(&cmd))?;
    clap_mangen::generate_to(cmd, dir.join("man"))?;
    println!("wrote {}", dir.display());
    Ok(())
}
