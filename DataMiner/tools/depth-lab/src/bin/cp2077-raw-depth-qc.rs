use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;

use depth_lab::{RawDepthQcContract, audit_cp2077_raw_depth_session};

fn usage() -> &'static str {
    "usage: cp2077-raw-depth-qc <short-smoke|coordinated-demo> <absolute-session-dir> <absolute-output-json>"
}

fn run() -> Result<bool, String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 3 {
        return Err(usage().into());
    }
    let contract = match args[0].as_str() {
        "short-smoke" => RawDepthQcContract::ShortSmoke,
        "pr405-local-30s" => RawDepthQcContract::Pr405Local30Seconds,
        "coordinated-demo" => RawDepthQcContract::CoordinatedDemo,
        _ => return Err(usage().into()),
    };
    let session = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    if !session.is_absolute() || !output.is_absolute() {
        return Err("session and output paths must be absolute".into());
    }
    let report = audit_cp2077_raw_depth_session(&session, contract)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .map_err(|error| format!("create raw depth QC report failed: {error}"))?;
    serde_json::to_writer_pretty(&mut file, &report)
        .map_err(|error| format!("serialize raw depth QC report failed: {error}"))?;
    file.write_all(b"\n")
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("sync raw depth QC report failed: {error}"))?;
    println!("{}", output.display());
    Ok(report.passed)
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
