//! Bounded, create-new offline review of prepared holstered v3 evidence summaries.
use depth_lab::camera_evidence::holstered::evidence_set::{self, EvidenceSet};
use depth_lab::camera_evidence::holstered::{ReviewInput, geometry_claims_sha256, review};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
};

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("CP2077 authoring review refused: {error}");
            std::process::exit(2);
        }
    }
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > limit {
        return Err("input is not a bounded nonempty regular file".into());
    }
    let mut bytes = Vec::new();
    f.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 != meta.len() {
        return Err("input size changed during read".into());
    }
    Ok(bytes)
}

fn run() -> Result<bool, String> {
    let mut paths = BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--input" | "--geometry-a" | "--geometry-b" | "--geometry" | "--out"
        ) {
            return Err(format!("unknown argument {key}"));
        }
        let value = args.next().ok_or("argument needs a path")?;
        if paths.insert(key, PathBuf::from(value)).is_some() {
            return Err("duplicate argument".into());
        }
    }
    let input=paths.get("--input").ok_or("usage: --input <summary.json> [--geometry-a <original.json>] [--geometry-b <original.json>] --out <new-report.json>")?;
    let output = paths.get("--out").ok_or("--out is required")?;
    if output.exists() {
        return Err("output already exists; never overwrite evidence".into());
    }
    let input_bytes = bounded_read(input, 2 * 1024 * 1024)?;
    let header: serde_json::Value =
        serde_json::from_slice(&input_bytes).map_err(|e| e.to_string())?;
    if header["schema"] == evidence_set::INPUT_SCHEMA {
        if paths.contains_key("--geometry-a") || paths.contains_key("--geometry-b") {
            return Err(
                "evidence-set uses one --geometry report with its original identity".into(),
            );
        }
        let payload: EvidenceSet =
            serde_json::from_slice(&input_bytes).map_err(|e| e.to_string())?;
        let geometry = paths
            .get("--geometry")
            .map(|p| bounded_read(p, 64 * 1024 * 1024))
            .transpose()?;
        let sha = |b: &[u8]| format!("sha256:{:x}", Sha256::digest(b));
        let report = evidence_set::review_set(&payload, geometry.as_deref(), sha)?;
        let digest = geometry.as_deref().map(sha);
        let claim = digest
            .as_deref()
            .map(|h| geometry_claims_sha256(&payload.geometry.evidence, h, sha))
            .transpose()?;
        publish(
            output,
            &serde_json::json!({"input_sha256":sha(&input_bytes),
            "computed_geometry_claims":{"process_launch_id":payload.geometry.evidence.process_launch_id,
                "report_sha256":digest,"claims_sha256":claim}, "review":report}),
        )?;
        println!(
            "ready_for_local_configuration={} non_trainable=true live_client_verified=false production_binding_authorized=false",
            report.ready_for_local_configuration
        );
        return Ok(report.ready_for_local_configuration);
    }
    if paths.contains_key("--geometry") {
        return Err("legacy launch review requires --geometry-a/--geometry-b".into());
    }
    let payload: ReviewInput = serde_json::from_slice(&input_bytes).map_err(|e| e.to_string())?;
    let geometry: Vec<Option<Vec<u8>>> = ["--geometry-a", "--geometry-b"]
        .iter()
        .map(|key| {
            paths
                .get(*key)
                .map(|p| bounded_read(p, 64 * 1024 * 1024))
                .transpose()
        })
        .collect::<Result<_, _>>()?;
    let sha = |b: &[u8]| format!("sha256:{:x}", Sha256::digest(b));
    let report = review(
        &payload,
        [geometry[0].as_deref(), geometry[1].as_deref()],
        sha,
    )?;
    // Expose the deterministic claim digest for an operator-prepared follow-up input.
    // Computing a digest neither changes the supplied attestation nor passes any gate.
    let claims = payload
        .launches
        .iter()
        .map(|launch| {
            let index = usize::from(launch.role == depth_lab::D3d12AuthoringLaunchRole::B);
            let digest = geometry[index].as_deref().map(sha);
            let claim = digest
                .as_deref()
                .map(|d| geometry_claims_sha256(launch, d, sha))
                .transpose()?;
            Ok(
                serde_json::json!({"process_launch_id":launch.process_launch_id,
            "report_sha256":digest,"claims_sha256":claim}),
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    publish(
        output,
        &serde_json::json!({
            "input_sha256":sha(&input_bytes),
            "geometry_report_sha256":geometry.iter().map(|b|b.as_deref().map(sha)).collect::<Vec<_>>(),
            "computed_geometry_claims":claims,
            "review":report
        }),
    )?;
    println!(
        "ready_for_human_review={} non_trainable=true production_binding_authorized=false",
        report.ready_for_human_review
    );
    Ok(report.ready_for_human_review)
}

fn publish(output: &Path, report: &serde_json::Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}
