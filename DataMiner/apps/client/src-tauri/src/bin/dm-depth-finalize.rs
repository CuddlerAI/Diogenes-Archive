use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dataminer_client_lib::depth_finalize_recovery::{RecoveryOptions, run};

fn main() -> ExitCode {
    let (options, result_path) = match parse_args() {
        Ok(parsed) => parsed,
        Err(error) => return fail(None, &error),
    };
    match run(&options) {
        Ok(report) => {
            let value = serde_json::json!({ "passed": true, "report": report });
            match write_result(&result_path, &value) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(Some(&result_path), &error),
            }
        }
        Err(error) => {
            let value = serde_json::json!({ "passed": false, "error": error });
            let _ = write_result(&result_path, &value);
            ExitCode::from(2)
        }
    }
}

fn parse_args() -> Result<(RecoveryOptions, PathBuf), String> {
    let mut journal_path = None;
    let mut journal_backup_path = None;
    let mut run_directory = None;
    let mut session_id = None;
    let mut run_id = None;
    let mut result_path = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if matches!(argument.as_str(), "--help" | "-h") {
            print_usage();
            std::process::exit(0);
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("{argument} requires a value"))?;
        match argument.as_str() {
            "--journal" => journal_path = Some(PathBuf::from(value)),
            "--journal-backup" => journal_backup_path = Some(PathBuf::from(value)),
            "--run-directory" => run_directory = Some(PathBuf::from(value)),
            "--session" => session_id = Some(value),
            "--run" => run_id = Some(value),
            "--result" => result_path = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok((
        RecoveryOptions {
            journal_path: journal_path.ok_or("--journal is required")?,
            journal_backup_path: journal_backup_path.ok_or("--journal-backup is required")?,
            run_directory: run_directory.ok_or("--run-directory is required")?,
            session_id: session_id.ok_or("--session is required")?,
            run_id: run_id.ok_or("--run is required")?,
        },
        result_path.ok_or("--result is required")?,
    ))
}

fn write_result(path: &Path, value: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}

fn fail(result_path: Option<&Path>, message: &str) -> ExitCode {
    if let Some(result_path) = result_path {
        let _ = write_result(
            result_path,
            &serde_json::json!({ "passed": false, "error": message }),
        );
    }
    eprintln!("{message}");
    print_usage();
    ExitCode::from(2)
}

fn print_usage() {
    eprintln!(
        "dm-depth-finalize --journal PATH --journal-backup PATH --run-directory PATH \
         --session ID --run ID --result PATH"
    );
}
