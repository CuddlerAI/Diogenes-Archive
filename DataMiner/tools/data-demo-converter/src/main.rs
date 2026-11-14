use std::env;
use std::path::PathBuf;

use anyhow::{Context, bail, ensure};
use data_demo_converter::{
    ComplianceProfile, convert_session_with_profile, inspect_sync_samples, reproject_session,
    set_control_policy, verify_dataset_with_profile,
};

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
        .context("missing command; use convert, verify, reproject, or inspect-mp4")?;

    match command.as_str() {
        "convert" => {
            let parsed = parse_flags(
                args,
                &[
                    "--session",
                    "--output",
                    "--profile",
                    "--consent-id",
                    "--waiver-ref",
                    "--control-vks",
                ],
            )?;
            let session = required_path(&parsed, "--session")?;
            let output = required_path(&parsed, "--output")?;
            let compliance = compliance_profile(&parsed)?;
            // Must match the policy the RAW working copy was reprojected under,
            // or the frames-vs-events cross-check rejects every segment.
            if let Some(control_vks) = flag_value(&parsed, "--control-vks") {
                set_control_policy(&parse_control_vks(control_vks)?);
            }
            let report = convert_session_with_profile(&session, &output, &compliance)?;
            println!(
                "converted session={} frames={} chunks={} profile={} output={}",
                report.session_id,
                report.frame_count,
                report.chunk_count,
                profile_label(&compliance),
                report.output.display()
            );
        }
        "verify" => {
            let parsed = parse_flags(
                args,
                &["--dataset", "--profile", "--consent-id", "--waiver-ref"],
            )?;
            let dataset = required_path(&parsed, "--dataset")?;
            let compliance = compliance_profile(&parsed)?;
            let report = verify_dataset_with_profile(&dataset, &compliance)?;
            println!(
                "verified session={} frames={} chunks={} profile={} dataset={}",
                report.session_id,
                report.frame_count,
                report.chunk_count,
                profile_label(&compliance),
                dataset.display()
            );
        }
        "reproject" => {
            let parsed = parse_flags(args, &["--session", "--control-vks"])?;
            let session = required_path(&parsed, "--session")?;
            let control_vks = parse_control_vks(
                flag_value(&parsed, "--control-vks").context("missing --control-vks")?,
            )?;
            let report = reproject_session(&session, &control_vks)?;
            println!("{}", serde_json::to_string(&report)?);
        }
        "inspect-mp4" => {
            let parsed = parse_flags(args, &["--path"])?;
            let path = required_path(&parsed, "--path")?;
            let sync = inspect_sync_samples(&path)?;
            println!("{}", serde_json::to_string(&sync)?);
        }
        "--help" | "-h" | "help" => print_help(),
        other => bail!("unknown command {other}; use convert, verify, reproject, or inspect-mp4"),
    }
    Ok(())
}

fn parse_control_vks(raw: &str) -> anyhow::Result<Vec<u16>> {
    raw.split(',')
        .map(|value| value.trim().parse::<u16>().context("invalid control VK"))
        .collect()
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

fn flag_value<'a>(parsed: &'a [(String, String)], flag: &str) -> Option<&'a str> {
    parsed
        .iter()
        .find(|(name, _)| name == flag)
        .map(|(_, value)| value.as_str())
}

fn required_path(parsed: &[(String, String)], flag: &str) -> anyhow::Result<PathBuf> {
    flag_value(parsed, flag)
        .map(PathBuf::from)
        .with_context(|| format!("missing {flag}"))
}

fn compliance_profile(parsed: &[(String, String)]) -> anyhow::Result<ComplianceProfile> {
    let profile = flag_value(parsed, "--profile").unwrap_or("local-debug");
    let consent = flag_value(parsed, "--consent-id");
    let waiver = flag_value(parsed, "--waiver-ref");
    match profile {
        "local-debug" => {
            ensure!(
                consent.is_none(),
                "--consent-id is only valid with --profile first-party-release"
            );
            ensure!(
                waiver.is_none(),
                "--waiver-ref is only valid with --profile attested-trainable"
            );
            Ok(ComplianceProfile::LocalDebugNonTrainable)
        }
        "first-party-release" => {
            ensure!(
                waiver.is_none(),
                "--waiver-ref is only valid with --profile attested-trainable"
            );
            let consent_id = consent
                .context("--profile first-party-release requires --consent-id")?
                .to_owned();
            Ok(ComplianceProfile::FirstPartyRelease { consent_id })
        }
        "attested-trainable" => {
            ensure!(
                consent.is_none(),
                "--consent-id is not valid with --profile attested-trainable: on the \
                 attested lane consent is carried by the WAIVER itself; pass \
                 --waiver-ref <waiver reference> instead"
            );
            let waiver_ref = waiver
                .context(
                    "--profile attested-trainable requires --waiver-ref \
                     (e.g. r2://diogenes-model-training/raw/v1/attested-trainable/WAIVER.json@2026-08-10)",
                )?
                .to_owned();
            Ok(ComplianceProfile::AttestedTrainable { waiver_ref })
        }
        other => bail!(
            "unknown profile {other}; use local-debug, first-party-release, or attested-trainable"
        ),
    }
}

fn profile_label(compliance: &ComplianceProfile) -> &'static str {
    match compliance {
        ComplianceProfile::LocalDebugNonTrainable => "local-debug",
        ComplianceProfile::FirstPartyRelease { .. } => "first-party-release",
        ComplianceProfile::AttestedTrainable { .. } => "attested-trainable",
    }
}

fn print_help() {
    println!(
        "dataminer-data-demo\n\n\
         Commands:\n\
           convert --session <ready-session-dir> --output <new-dataset-dir>\n\
                   [--profile local-debug|first-party-release|attested-trainable]\n\
                   [--consent-id <CNS-...>] [--waiver-ref <waiver reference>]\n\
                   [--control-vks 18,119,121]   (must match the reprojection policy)\n\
           verify --dataset <dataset-dir>\n\
                  [--profile local-debug|first-party-release|attested-trainable]\n\
                  [--consent-id <CNS-...>] [--waiver-ref <waiver reference>]\n\
           reproject --session <raw-session-working-copy> --control-vks 18,119,121\n\
           inspect-mp4 --path <video.mp4>\n\n\
         The default profile is local-debug (non-trainable, non-certified).\n\
         first-party-release requires a consent id and accepts only consent-stamped\n\
         RAW sessions (consented_ready segments carrying the same consent id).\n\
         attested-trainable requires --waiver-ref (forbids --consent-id: on that lane\n\
         consent is carried by the WAIVER) and accepts truthful non-trainable RAW\n\
         (ready/non_trainable_ready segments, consent_id null, dev_non_trainable\n\
         catalog); it stamps rights=first_party_attested, is_trainable=true,\n\
         production_certified=false, with attestation.waiver_ref recorded."
    );
}
