use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("CP2077 PR405 local package refused: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut session = None;
    let mut manifest = None;
    let mut out = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{argument} requires one path"))?;
        match argument.as_str() {
            "--session" => session = Some(PathBuf::from(value)),
            "--manifest" => manifest = Some(PathBuf::from(value)),
            "--out" => out = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let session = session.ok_or("usage: cp2077-pr405-local-package --session <absolute-dir> --manifest <absolute-json> --out <absolute-new-dir>")?;
    let manifest = manifest.ok_or("missing --manifest")?;
    let out = out.ok_or("missing --out")?;
    let report =
        depth_lab::cp2077_pr405_package::package_cp2077_pr405_local(&session, &manifest, &out)?;
    println!(
        "CP2077 PR405 local package sealed: rgb_frames={} depth_frames={} passed={}",
        report.rgb_frames, report.depth_frames, report.passed
    );
    Ok(())
}
