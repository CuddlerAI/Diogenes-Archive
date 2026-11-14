use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("CP2077 metric evidence refused: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut values = std::collections::BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{argument} requires one path"))?;
        if !matches!(
            argument.as_str(),
            "--session" | "--pose" | "--frame-evidence" | "--hud-observations" | "--out"
        ) {
            return Err(format!("unknown argument {argument}"));
        }
        if values.insert(argument, PathBuf::from(value)).is_some() {
            return Err("duplicate argument".into());
        }
    }
    let get = |name: &str| {
        values.get(name).cloned().ok_or_else(|| {
            "usage: cp2077-metric-evidence --session <absolute-dir> --pose <absolute-jsonl> --frame-evidence <absolute-jsonl> --hud-observations <absolute-json> --out <absolute-new-json>".to_string()
        })
    };
    let report = depth_lab::cp2077_metric_evidence::generate_cp2077_metric_evidence(
        &get("--session")?,
        &get("--pose")?,
        &get("--frame-evidence")?,
        &get("--hud-observations")?,
        &get("--out")?,
    )?;
    println!(
        "CP2077 metric evidence sealed: source_a={:.6} source_b={:.6} samples={}",
        report.cross_check.source_a,
        report.cross_check.source_b,
        report.source_b.samples.len()
    );
    Ok(())
}
