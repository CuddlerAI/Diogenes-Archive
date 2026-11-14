use std::env;
use std::path::PathBuf;

use anyhow::{Context, bail};
use spec_export::convert_session;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = env::args_os();
    let _program = args.next();
    let command = args
        .next()
        .and_then(|value| value.into_string().ok())
        .context("missing command; use convert")?;

    match command.as_str() {
        "convert" => {
            let session = required_path(&mut args, "--session")?;
            let output = required_path(&mut args, "--output")?;
            ensure_no_extra(args)?;
            let report = convert_session(&session, &output)?;
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            println!(
                "exported session={} frames={} actions={} heartbeats={} output={}",
                report.session_id,
                report.frames,
                report.actions,
                report.heartbeats,
                report.output.display()
            );
        }
        "--help" | "-h" | "help" => print_help(),
        other => bail!("unknown command {other}; use convert"),
    }
    Ok(())
}

fn required_path(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    expected: &str,
) -> anyhow::Result<PathBuf> {
    let flag = args
        .next()
        .and_then(|value| value.into_string().ok())
        .with_context(|| format!("missing {expected}"))?;
    if flag != expected {
        bail!("expected {expected}, got {flag}");
    }
    args.next()
        .map(PathBuf::from)
        .with_context(|| format!("missing value for {expected}"))
}

fn ensure_no_extra(mut args: impl Iterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    if let Some(value) = args.next() {
        bail!("unexpected argument {}", value.to_string_lossy());
    }
    Ok(())
}

fn print_help() {
    println!(
        "dataminer-spec-export\n\n\
         Commands:\n\
           convert --session <dm-1-ready-session-dir> --output <spec-v1-session-dir>"
    );
}
