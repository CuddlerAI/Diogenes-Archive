use std::io::{Read, Write};
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
    let mut args = std::env::args_os();
    let _program = args.next();
    let config = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let launcher_stdout = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let executable = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let probe = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let d3d12_report = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let pose_report = args.next().map(PathBuf::from).ok_or_else(usage)?;
    let output = args.next().map(PathBuf::from).ok_or_else(usage)?;
    if args.next().is_some() {
        return Err(usage());
    }
    for path in [
        &config,
        &launcher_stdout,
        &executable,
        &probe,
        &d3d12_report,
        &pose_report,
        &output,
    ] {
        if !path.is_absolute() {
            return Err("all local discovery seal paths must be absolute".into());
        }
    }
    validate_evidence_layout(&config, &d3d12_report, &pose_report, &output)?;

    let config_bytes = read_bounded(&config, "discovery config")?;
    let launcher_stdout_bytes = read_bounded(&launcher_stdout, "launcher stdout")?;
    let d3d12_report_bytes = read_bounded(&d3d12_report, "D3D12 discovery report")?;
    let pose_report_bytes = read_bounded(&pose_report, "pose discovery report")?;
    let executable_sha256 = sha256_file(&executable, "CP2077 executable")?;
    let probe_sha256 = sha256_file(&probe, "resident probe DLL")?;

    let marker = depth_lab::seal_cp2077_local_discovery(
        &config_bytes,
        &launcher_stdout_bytes,
        &executable_sha256,
        &probe_sha256,
        &d3d12_report_bytes,
        &pose_report_bytes,
        sha256_bytes,
    )
    .map_err(|error| error.to_string())?;
    let mut bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| format!("serialize local discovery marker failed: {error}"))?;
    bytes.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .map_err(|error| format!("create output {} failed: {error}", output.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write output {} failed: {error}", output.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync output {} failed: {error}", output.display()))?;
    println!("{}", output.display());
    Ok(())
}

fn usage() -> String {
    "usage: cp2077-local-discovery-seal <absolute-config> <absolute-launcher-stdout> <absolute-cp2077-exe> <absolute-probe-dll> <absolute-d3d12-report> <absolute-pose-report> <absolute-output>".into()
}

fn validate_evidence_layout(
    config: &Path,
    d3d12_report: &Path,
    pose_report: &Path,
    output: &Path,
) -> Result<(), String> {
    let config_parent = canonical_parent(config, "discovery config")?;
    let d3d12_parent = canonical_parent(d3d12_report, "D3D12 report")?;
    let pose_parent = canonical_parent(pose_report, "pose report")?;
    let output_parent = canonical_parent(output, "output")?;
    if config_parent != d3d12_parent
        || config_parent != pose_parent
        || config_parent != output_parent
        || config.file_name().and_then(|name| name.to_str()) != Some("discovery-config.json")
        || d3d12_report.file_name().and_then(|name| name.to_str()) != Some("d3d12-observation.json")
        || pose_report.file_name().and_then(|name| name.to_str())
            != Some("cp2077-pose-runtime-observation.json")
        || output.file_name().and_then(|name| name.to_str())
            != Some("local-discovery-report-sealed.json")
    {
        return Err(
            "config, both exact-named reports, and exact-named output must share one evidence directory"
                .into(),
        );
    }
    Ok(())
}

fn canonical_parent(path: &Path, label: &str) -> Result<PathBuf, String> {
    path.parent()
        .ok_or_else(|| format!("{label} has no parent"))?
        .canonicalize()
        .map_err(|error| format!("resolve {label} parent failed: {error}"))
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

fn sha256_file(path: &Path, label: &str) -> Result<String, String> {
    let metadata = path
        .metadata()
        .map_err(|error| format!("inspect {label} failed: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{label} is not a regular file"));
    }
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {label} failed: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {label} failed: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_layout_requires_the_exact_session_filenames() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("discovery-config.json");
        let d3d12 = root.path().join("d3d12-observation.json");
        let pose = root.path().join("cp2077-pose-runtime-observation.json");
        let output = root.path().join("local-discovery-report-sealed.json");
        for path in [&config, &d3d12, &pose] {
            std::fs::write(path, b"{}").unwrap();
        }
        assert!(validate_evidence_layout(&config, &d3d12, &pose, &output).is_ok());
        assert!(
            validate_evidence_layout(
                &config,
                &d3d12,
                &pose,
                &root.path().join("discovery-report-sealed.json")
            )
            .is_err()
        );
    }
}
