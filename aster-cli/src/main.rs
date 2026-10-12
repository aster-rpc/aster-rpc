use std::process::ExitCode;

use aster_cli::Cli;
use aster_cli_kit::ctx::json_requested;
use aster_cli_kit::out::Styler;
use aster_cli_kit::{Ctx, Error, ErrorKind};
use clap::Parser;
use clap::error::ErrorKind as ClapKind;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    let json = json_requested(&args);
    let plain = Styler::new(false);
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), ClapKind::DisplayHelp | ClapKind::DisplayVersion) || !json => {
            let _ = e.print();
            return ExitCode::from(e.exit_code() as u8);
        }
        Err(e) => {
            let rendered = e.to_string();
            let message = rendered.lines().next().unwrap_or_default();
            Error::usage(message.trim_start_matches("error: ")).report(true, plain);
            return ExitCode::from(ErrorKind::Usage.exit_code());
        }
    };
    let ctx = match Ctx::new(&cli.global) {
        Ok(ctx) => ctx,
        Err(e) => {
            e.report(json, plain);
            return ExitCode::from(e.kind().exit_code());
        }
    };
    let outcome = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::from)
        .and_then(|rt| rt.block_on(aster_cli::run(&ctx, cli.command)));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            e.report(ctx.json, ctx.err());
            let code = e
                .details()
                .and_then(|value| value.get("remote_job_exit_code"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|code| u8::try_from(code).ok())
                .filter(|code| *code != 0)
                .unwrap_or_else(|| e.kind().exit_code());
            ExitCode::from(code)
        }
    }
}
