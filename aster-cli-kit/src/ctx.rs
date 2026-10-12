//! Global options and the per-invocation context every command receives.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::{Args, ValueEnum};

use crate::error::{Error, Result};
use crate::out::Styler;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

/// Options accepted by every command; flatten into the top-level parser.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Aster state directory [default: ~/.aster]
    #[arg(long, global = true, env = "ASTER_HOME", value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// Profile to act as, instead of the active one
    #[arg(long, global = true, env = "ASTER_PROFILE", value_name = "NAME")]
    pub profile: Option<String>,

    /// Write machine-readable JSON to stdout
    #[arg(long, global = true)]
    pub json: bool,

    /// When to use colour
    #[arg(long, global = true, value_enum, value_name = "WHEN", default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,
}

#[derive(Debug, Clone)]
pub struct Ctx {
    pub home: PathBuf,
    /// `--profile` / `ASTER_PROFILE`; resolve with
    /// `aster_config::Config::selected_profile_name`.
    pub profile: Option<String>,
    pub json: bool,
    pub(crate) stdin_tty: bool,
    pub(crate) stdout_tty: bool,
    pub(crate) stderr_tty: bool,
    color: ColorChoice,
}

impl Ctx {
    pub fn new(args: &GlobalArgs) -> Result<Self> {
        let home = match &args.home {
            Some(h) => h.clone(),
            None => aster_config::default_home().ok_or_else(|| {
                Error::usage("cannot determine the home directory")
                    .with_hint("pass --home or set ASTER_HOME")
            })?,
        };
        Ok(Self {
            home,
            profile: args.profile.clone(),
            json: args.json,
            stdin_tty: std::io::stdin().is_terminal(),
            stdout_tty: std::io::stdout().is_terminal(),
            stderr_tty: std::io::stderr().is_terminal(),
            color: args.color,
        })
    }

    /// Context for MCP tool calls: JSON, no terminal, no colour.
    pub fn for_tools(&self) -> Self {
        Self {
            home: self.home.clone(),
            profile: self.profile.clone(),
            json: true,
            stdin_tty: false,
            stdout_tty: false,
            stderr_tty: false,
            color: ColorChoice::Never,
        }
    }

    /// Styling for stdout.
    pub fn out(&self) -> Styler {
        Styler::new(self.colour(self.stdout_tty))
    }

    /// Styling for stderr (errors, prompts, progress).
    pub fn err(&self) -> Styler {
        Styler::new(self.colour(self.stderr_tty))
    }

    /// Whether a y/N question can be asked: never with `--json` or without a
    /// terminal on stdin and stderr.
    pub fn can_prompt(&self) -> bool {
        !self.json && self.stdin_tty && self.stderr_tty
    }

    /// Whether a full-screen live view may take over the terminal.
    pub fn can_draw_live(&self) -> bool {
        !self.json && self.stdout_tty && self.stdin_tty
    }

    fn colour(&self, tty: bool) -> bool {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        match self.color {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto if set("NO_COLOR") => false,
            ColorChoice::Auto if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") => {
                true
            }
            ColorChoice::Auto => tty,
        }
    }
}

/// Whether `--json` appears before any `--`, for reporting argument errors
/// before parsing has succeeded.
pub fn json_requested(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--json")
}
