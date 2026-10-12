//! Conventions shared by the `aster` binary and every command family it
//! hosts (profiles, Exec, …). A family crate depends on this crate, not on the
//! binary, and exports three things the binary composes:
//!
//! - a clap `Subcommand` (or `Args`) type for its commands;
//! - an `async fn run(&Ctx, Command) -> Result<()>` dispatcher;
//! - `fn tools() -> Vec<Tool>` for the shared MCP server (`aster mcp`).
//!
//! Commands and tools call the same library functions; neither wraps the other.
//!
//! Conventions, enforced by the types here:
//!
//! - **Exit codes** come from [`ErrorKind`]: 0 success, 1 failure, 2 usage,
//!   3 not found, 4 denied, 5 unavailable, 130 cancelled.
//! - **Errors** go to stderr: `error: …`, then `  caused by: …` and
//!   `  hint: …` lines. With `--json`, a single `{"error": {...}}` object.
//! - **`--json`** writes exactly one JSON object to stdout (pretty on a
//!   terminal, compact otherwise); streams are NDJSON with an `event` field.
//!   The top level is always an object so fields can be added.
//! - **Colour** only on a terminal, per stream; `NO_COLOR` disables it and
//!   `CLICOLOR_FORCE` or `--color always` forces it.
//! - **Without a terminal** nothing prompts (pass `--yes`) and live views fall
//!   back to plain lines or NDJSON.

pub mod ctx;
pub mod docs;
pub mod error;
pub mod mcp;
pub mod out;
pub mod prompt;
pub mod tool;

pub use ctx::{ColorChoice, Ctx, GlobalArgs};
pub use error::{Error, ErrorKind, Result};
pub use out::Report;
pub use tool::Tool;
