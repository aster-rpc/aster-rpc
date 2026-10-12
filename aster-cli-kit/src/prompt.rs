//! Confirmation prompts. Without a terminal they refuse rather than guess, so
//! scripts and agents must pass `--yes` explicitly.

use std::io::{BufRead, Write};

use crate::ctx::Ctx;
use crate::error::{Error, Result};

/// Ask a y/N question on stderr; anything but `y`/`yes` is no.
pub fn confirm(ctx: &Ctx, question: &str) -> Result<bool> {
    if !ctx.can_prompt() {
        return Err(Error::usage("confirmation needs a terminal")
            .with_hint("pass --yes to proceed non-interactively"));
    }
    let mut err = std::io::stderr().lock();
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
