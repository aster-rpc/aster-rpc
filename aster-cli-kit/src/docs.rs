//! Command reference in Markdown, generated from the clap tree, so the docs
//! site, `--help`, completions and the shell all share one definition. Man
//! pages come from `clap_mangen` in the binary's `docgen` example instead,
//! keeping it out of the shipped binary.

use std::fmt::Write;

use clap::{Arg, Command};

/// One section per visible command, depth first. Global options are listed
/// once, under the root.
pub fn markdown(root: &Command) -> String {
    let mut root = root.clone();
    root.build();
    let mut out = String::new();
    let _ = writeln!(out, "# `{}` command reference\n", root.get_name());
    let _ = writeln!(
        out,
        "Generated from the command definitions by `cargo run -p aster-cli --example docgen`; do not edit.\n"
    );
    section(&mut out, &mut root, true);
    out
}

fn section(out: &mut String, cmd: &mut Command, root: bool) {
    let path = cmd.get_bin_name().unwrap_or(cmd.get_name()).to_string();
    let _ = writeln!(out, "{} `{path}`\n", if root { "##" } else { "###" });
    if let Some(about) = cmd.get_long_about().or(cmd.get_about()) {
        let _ = writeln!(out, "{about}\n");
    }
    let usage = cmd.render_usage().to_string();
    let _ = writeln!(out, "```text\n{usage}\n```\n");

    let args: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !matches!(a.get_id().as_str(), "help" | "version"))
        .filter(|a| root || !a.is_global_set())
        .collect();
    if !args.is_empty() {
        let _ = writeln!(out, "| Argument | Description |\n| --- | --- |");
        for arg in args {
            let _ = writeln!(out, "| `{}` | {} |", arg_name(arg), arg_help(arg));
        }
        out.push('\n');
    }

    let subs: Vec<String> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set() && s.get_name() != "help")
        .map(|s| s.get_name().to_string())
        .collect();
    if !subs.is_empty() {
        let _ = writeln!(
            out,
            "Subcommands: {}\n",
            subs.iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for name in subs {
        if let Some(sub) = cmd.find_subcommand_mut(&name) {
            section(out, sub, false);
        }
    }
}

fn arg_name(arg: &Arg) -> String {
    let value = arg
        .get_value_names()
        .and_then(|v| v.first())
        .map(|v| v.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase());
    let takes_value = arg.get_num_args().is_some_and(|n| n.takes_values());
    match (arg.get_long(), arg.get_short()) {
        (Some(l), Some(s)) if takes_value => format!("-{s}, --{l} <{value}>"),
        (Some(l), Some(s)) => format!("-{s}, --{l}"),
        (Some(l), None) if takes_value => format!("--{l} <{value}>"),
        (Some(l), None) => format!("--{l}"),
        (None, _) if arg.is_required_set() => format!("<{value}>"),
        (None, _) => format!("[{value}]"),
    }
}

fn arg_help(arg: &Arg) -> String {
    let mut help = arg
        .get_long_help()
        .or(arg.get_help())
        .map(|h| h.to_string())
        .unwrap_or_default();
    if !help.is_empty() && !help.ends_with(['.', ']', ')']) {
        help.push('.');
    }
    let values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|v| !v.is_hide_set())
        .map(|v| format!("`{}`", v.get_name()))
        .collect();
    if !values.is_empty() {
        let _ = write!(help, " One of {}.", values.join(", "));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|v| v.to_string_lossy().into_owned())
        .collect();
    let takes_value = arg.get_num_args().is_some_and(|n| n.takes_values());
    if takes_value && !defaults.is_empty() {
        let _ = write!(help, " Default: `{}`.", defaults.join(","));
    }
    if let Some(env) = arg.get_env() {
        let _ = write!(help, " Environment: `{}`.", env.to_string_lossy());
    }
    help.trim().replace('|', "\\|").replace('\n', " ")
}
