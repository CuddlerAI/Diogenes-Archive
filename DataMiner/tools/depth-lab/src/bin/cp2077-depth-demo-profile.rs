use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const MAX_EVIDENCE_BYTES: u64 = 4 * 1024 * 1024;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let report = args.first().cloned().map(PathBuf::from).ok_or_else(usage)?;
    let sealed = args.get(1).cloned().map(PathBuf::from).ok_or_else(usage)?;
    let candidate_id = args
        .get(2)
        .cloned()
        .ok_or_else(usage)?
        .into_string()
        .map_err(|_| "candidate id is not UTF-8".to_string())?;
    let output = args.get(3).cloned().map(PathBuf::from).ok_or_else(usage)?;
    let (capture_contract, duration_ms) = match args.get(4).and_then(|value| value.to_str()) {
        None => (
            depth_lab::ISSUE735_LOCAL_CONTRACT,
            depth_lab::ISSUE735_LOCAL_DURATION_MS,
        ),
        Some("--contract")
            if args.get(6).and_then(|value| value.to_str()) == Some("--duration-seconds") =>
        {
            let contract = args
                .get(5)
                .and_then(|value| value.to_str())
                .ok_or_else(usage)?;
            let seconds = args
                .get(7)
                .and_then(|value| value.to_str())
                .ok_or_else(usage)?
                .parse::<u64>()
                .map_err(|_| usage())?;
            (contract, seconds.saturating_mul(1_000))
        }
        _ => return Err(usage()),
    };
    if !matches!(args.len(), 4 | 8) {
        return Err(usage());
    }
    depth_lab::validate_capture_contract(capture_contract, duration_ms)?;
    if !report.is_absolute() || !sealed.is_absolute() || !output.is_absolute() {
        return Err("report, sealed marker, and output paths must be absolute".into());
    }
    let report_bytes = read_bounded(&report, "D3D12 discovery report")?;
    let sealed_bytes = read_bounded(&sealed, "sealed discovery marker")?;
    let mut profile = depth_lab::author_cp2077_raw_depth_demo(
        &report_bytes,
        &sealed_bytes,
        &candidate_id,
        sha256_bytes,
    )
    .map_err(|error| error.to_string())?;
    profile.capture_contract = capture_contract.into();
    profile.duration_ms = duration_ms;
    profile.validate()?;
    let mut bytes = serde_json::to_vec_pretty(&profile)
        .map_err(|error| format!("serialize raw depth demo profile failed: {error}"))?;
    bytes.push(b'\n');
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create output directory failed: {error}"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .map_err(|error| format!("create output {} failed: {error}", output.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write output {} failed: {error}", output.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync output {} failed: {error}", output.display()))?;
    eprintln!(
        "authored explicit operator candidate {} as local non-trainable raw smoke only; recording_ready=false",
        profile.selector.candidate_id
    );
    println!("{}", output.display());
    Ok(())
}

fn usage() -> String {
    "usage: cp2077-depth-demo-profile <absolute-d3d12-report> <absolute-sealed-marker> <explicit-operator-candidate-id> <absolute-output> [--contract issue735_local|pr405_local --duration-seconds 64|30]".into()
}

fn read_bounded(path: &Path, label: &str) -> Result<Vec<u8>, String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("inspect {label} failed: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_EVIDENCE_BYTES {
        return Err(format!("{label} is not a bounded regular file"));
    }
    std::fs::read(path).map_err(|error| format!("read {label} failed: {error}"))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
