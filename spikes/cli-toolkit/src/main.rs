//! Spike for docs/_internal/working_ideas/aster-cli-rust.md "First task".
//! Not product code; see README.md for what each part evaluates.

mod config;
mod keystore;
mod mcp;
mod out;
mod profile;
mod shell;
mod watch;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "aster",
    version,
    about = "Aster command-line tools (toolkit spike)"
)]
pub struct Cli {
    /// Aster state directory.
    #[arg(long, global = true, env = "ASTER_HOME", value_name = "DIR")]
    aster_home: Option<PathBuf>,

    /// Emit machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage operator profiles.
    #[command(subcommand)]
    Profile(profile::ProfileCommand),
    /// Interactive shell.
    Shell,
    /// Live view of a simulated job (stand-in for `aster exec watch`).
    Watch {
        /// Number of output lines the simulated job writes.
        #[arg(long, default_value_t = 40)]
        lines: u32,
        /// Delay between lines in milliseconds.
        #[arg(long, default_value_t = 80)]
        interval_ms: u64,
    },
    /// Serve MCP over stdio.
    Mcp,
    /// Print shell completion script.
    Completions { shell: clap_complete::Shell },
}

pub struct Ctx {
    pub home: PathBuf,
    pub json: bool,
    pub interactive: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse(), true) {
        Ok(code) => code,
        Err(e) => {
            out::error(&format!("{e:#}"));
            ExitCode::from(1)
        }
    }
}

pub fn run(cli: Cli, top_level: bool) -> anyhow::Result<ExitCode> {
    let home = match cli.aster_home {
        Some(h) => h,
        None => std::env::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine home directory; pass --aster-home"))?
            .join(".aster"),
    };
    let ctx = Ctx {
        home,
        json: cli.json,
        interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
    };
    match cli.command {
        Command::Profile(cmd) => profile::run(&ctx, cmd),
        Command::Shell if top_level => shell::run(&ctx),
        Command::Watch { lines, interval_ms } => watch::run(&ctx, lines, interval_ms),
        Command::Mcp if top_level => mcp::run(&ctx),
        Command::Shell | Command::Mcp => anyhow::bail!("not available inside the shell"),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "aster", &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
    }
}
