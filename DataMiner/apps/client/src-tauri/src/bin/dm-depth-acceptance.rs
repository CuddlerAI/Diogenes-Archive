use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use dataminer_client_lib::issue504_acceptance::{AcceptanceOptions, run};

fn main() -> ExitCode {
    let mut options = AcceptanceOptions::production(default_app_data_root());
    let mut output: Option<PathBuf> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let value = match argument.as_str() {
            "--app-data" | "--session" | "--api-base-url" | "--wait-seconds" | "--output" => {
                match arguments.next() {
                    Some(value) => value,
                    None => return usage_error(&format!("{argument} requires a value")),
                }
            }
            "--help" | "-h" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            _ => return usage_error("unknown argument"),
        };
        match argument.as_str() {
            "--app-data" => options.app_data_root = PathBuf::from(value),
            "--session" => options.session_id = Some(value),
            "--api-base-url" => options.api_base_url = value,
            "--wait-seconds" => {
                let seconds = match value.parse::<u64>() {
                    Ok(seconds) if seconds <= 3_600 => seconds,
                    _ => return usage_error("--wait-seconds must be between 0 and 3600"),
                };
                options.wait = Duration::from_secs(seconds);
            }
            "--output" => output = Some(PathBuf::from(value)),
            _ => unreachable!(),
        }
    }

    let report = run(&options);
    let json = match serde_json::to_string_pretty(&report) {
        Ok(json) => json,
        Err(_) => return ExitCode::from(2),
    };
    println!("{json}");
    if let Some(path) = output
        && write_report(&path, &json).is_err()
    {
        eprintln!("failed to write acceptance report");
        return ExitCode::from(2);
    }
    if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn default_app_data_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("com.diogeneslab.dataminer")
}

fn write_report(path: &std::path::Path, json: &str) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, format!("{json}\n"))?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temporary, path)
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("{message}");
    print_usage();
    ExitCode::from(2)
}

fn print_usage() {
    eprintln!(
        "dm-depth-acceptance [--app-data PATH] [--session ID] [--api-base-url URL] \\\n         [--wait-seconds 0..3600] [--output REPORT.json]"
    );
}
