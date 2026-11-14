use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("CP2077 metric promotion refused: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let mut promotion = None;
    let mut private_session = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("{argument} requires one path"))?;
        match argument.as_str() {
            "--promotion" if promotion.is_none() => promotion = Some(PathBuf::from(value)),
            "--private-session" if private_session.is_none() => {
                private_session = Some(PathBuf::from(value));
            }
            "--promotion" | "--private-session" => return Err("duplicate argument".into()),
            _ => return Err(format!("unknown argument {argument}")),
        }
    }
    let promotion = promotion.ok_or_else(|| {
        "usage: cp2077-metric-promotion --promotion <json> [--private-session <dir>]".to_string()
    })?;
    let validation = match private_session {
        Some(session) => depth_lab::cp2077_metric_promotion::verify_private_metric_promotion(
            &promotion, &session,
        )?,
        None => depth_lab::cp2077_metric_promotion::verify_metric_promotion(&promotion)?,
    };
    println!(
        "CP2077 metric promotion verified: build={} scale={:.6} private_inputs_verified={}",
        validation.steam_build_id,
        validation.adopted_metres_per_world_unit,
        validation.private_inputs_verified
    );
    Ok(())
}
