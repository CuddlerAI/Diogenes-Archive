use std::env;
use std::path::PathBuf;

use anyhow::{Context, bail, ensure};
use dataset_normalizer::{FfmpegBackend, normalize, prepare, verify};

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
        .context("missing command; use plan, normalize, or verify")?;
    match command.as_str() {
        "plan" => {
            let flags = parse_flags(args, &["--request", "--spec"])?;
            let prepared = prepare(
                required_path(&flags, "--request")?,
                required_path(&flags, "--spec")?,
            )?;
            println!(
                "{}",
                serde_json::json!({
                    "session_id": prepared.request.session_id,
                    "spec_id": prepared.spec.spec.spec_id,
                    "spec_sha256": prepared.spec.sha256,
                    "frame_plan_sha256": prepared.plan_sha256,
                    "frames": prepared.plan.len(),
                    "labels": prepared.labels.len(),
                    "runs": prepared.runs.len(),
                    "duplicates": prepared.plan.iter().filter(|row| row.duplicate).count(),
                    "trimmed_source_frames": prepared.runs.iter()
                        .map(|run| u64::from(run.trimmed_source_frames)).sum::<u64>(),
                    "non_trainable": true
                })
            );
        }
        "normalize" => {
            let flags = parse_flags(
                args,
                &[
                    "--request",
                    "--spec",
                    "--output",
                    "--ffmpeg",
                    "--ffmpeg-sha256",
                ],
            )?;
            let backend = FfmpegBackend::pinned(
                required_path(&flags, "--ffmpeg")?,
                required_value(&flags, "--ffmpeg-sha256")?,
            )?;
            let report = normalize(
                required_path(&flags, "--request")?,
                required_path(&flags, "--spec")?,
                required_path(&flags, "--output")?,
                &backend,
            )?;
            println!("{}", serde_json::to_string(&report)?);
        }
        "verify" => {
            let flags = parse_flags(args, &["--dataset"])?;
            let report = verify(required_path(&flags, "--dataset")?)?;
            println!("{}", serde_json::to_string(&report)?);
        }
        "--help" | "-h" | "help" => print_help(),
        other => bail!("unknown command {other}; use plan, normalize, or verify"),
    }
    Ok(())
}

fn parse_flags(
    args: impl Iterator<Item = std::ffi::OsString>,
    allowed: &[&str],
) -> anyhow::Result<Vec<(String, String)>> {
    let mut parsed = Vec::new();
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let flag = flag
            .into_string()
            .map_err(|value| anyhow::anyhow!("non UTF-8 argument {}", value.to_string_lossy()))?;
        ensure!(
            allowed.contains(&flag.as_str()),
            "unexpected argument {flag}"
        );
        ensure!(
            !parsed.iter().any(|(name, _)| name == &flag),
            "duplicate argument {flag}"
        );
        let value = args
            .next()
            .and_then(|value| value.into_string().ok())
            .with_context(|| format!("missing value for {flag}"))?;
        parsed.push((flag, value));
    }
    Ok(parsed)
}

fn required_value<'a>(flags: &'a [(String, String)], flag: &str) -> anyhow::Result<&'a str> {
    flags
        .iter()
        .find(|(name, _)| name == flag)
        .map(|(_, value)| value.as_str())
        .with_context(|| format!("missing {flag}"))
}

fn required_path(flags: &[(String, String)], flag: &str) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(required_value(flags, flag)?))
}

fn print_help() {
    println!(
        "dataminer-dataset-normalizer\n\n\
         Commands:\n\
           plan --request <request.json> --spec <normalization-spec.json>\n\
           normalize --request <request.json> --spec <normalization-spec.json>\n\
                     --output <new-dir> --ffmpeg <ffmpeg.exe>\n\
                     --ffmpeg-sha256 sha256:<64-hex>\n\
           verify --dataset <normalized-dir>\n\n\
         v1 accepts only exact 60/1 -> 30/1, 20/1, or 12/1. Outputs are\n\
         always non-trainable; FramePair and downstream duplicate policy remain deferred."
    );
}
