//! Minimal interactive shell on rustyline: completion derived from the clap
//! command tree and dispatch through the same parser as the one-shot CLI.

use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};

use crate::{Cli, Ctx, out};

/// Returns (replace_from, candidates) for the word under the cursor.
pub fn complete(line: &str, pos: usize) -> (usize, Vec<(String, Option<String>)>) {
    let head = &line[..pos];
    let start = head.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let prefix = &head[start..];
    let mut cmd = Cli::command();
    for word in head[..start].split_whitespace() {
        match cmd.find_subcommand(word) {
            Some(sub) => cmd = sub.clone(),
            None => break,
        }
    }
    let mut found = Vec::new();
    if prefix.starts_with('-') {
        for arg in cmd.get_arguments() {
            if let Some(long) = arg
                .get_long()
                .map(|l| format!("--{l}"))
                .filter(|f| f.starts_with(prefix))
            {
                found.push((long, arg.get_help().map(ToString::to_string)));
            }
        }
    } else {
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            if name.starts_with(prefix) && !matches!(name, "help" | "shell" | "mcp") {
                found.push((name.to_string(), sub.get_about().map(ToString::to_string)));
            }
        }
    }
    (start, found)
}

struct ClapHelper;
impl Completer for ClapHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, items) = complete(line, pos);
        let pairs = items
            .into_iter()
            .map(|(v, d)| Pair {
                display: d.map_or_else(|| v.clone(), |d| format!("{v:<12} {d}")),
                replacement: format!("{v} "),
            })
            .collect();
        Ok((start, pairs))
    }
}
impl Hinter for ClapHelper {
    type Hint = String;
}
impl Highlighter for ClapHelper {}
impl Validator for ClapHelper {}
impl Helper for ClapHelper {}

fn dispatch(ctx: &Ctx, line: &str) {
    let Some(words) = shlex::split(line) else {
        out::error("unbalanced quotes");
        return;
    };
    if words.is_empty() {
        return;
    }
    let mut argv = vec![
        "aster".to_string(),
        "--aster-home".into(),
        ctx.home.display().to_string(),
    ];
    if ctx.json {
        argv.push("--json".into());
    }
    argv.extend(words);
    match Cli::try_parse_from(argv) {
        Ok(cli) => {
            if let Err(e) = crate::run(cli, false) {
                out::error(&format!("{e:#}"));
            }
        }
        Err(e) => {
            let _ = e.print();
        }
    }
}

pub fn run(ctx: &Ctx) -> Result<ExitCode> {
    if !ctx.interactive {
        anyhow::bail!("the shell needs a terminal; run commands directly instead");
    }
    let history_path = ctx.home.join("shell_history");
    let mut rl: Editor<ClapHelper, FileHistory> = Editor::new()?;
    rl.set_helper(Some(ClapHelper));
    let _ = rl.load_history(&history_path);
    loop {
        match rl.readline("aster> ") {
            Ok(line) if matches!(line.trim(), "exit" | "quit") => break,
            Ok(line) => {
                rl.add_history_entry(line.as_str())?;
                dispatch(ctx, &line);
            }
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    std::fs::create_dir_all(&ctx.home)?;
    rl.save_history(&history_path)?;
    Ok(ExitCode::SUCCESS)
}
