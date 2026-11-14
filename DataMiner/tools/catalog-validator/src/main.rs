use std::path::PathBuf;

use anyhow::{Context, ensure};

// The imported client source refers to `capture_core::*`. Alias this portable
// validator crate under that name and reuse capture-core's mapping/profile
// source files without compiling its Win32 runtime modules.
extern crate self as capture_core;

#[allow(dead_code)]
#[path = "../../../crates/capture-core/src/audio_contract.rs"]
mod audio_contract;
#[allow(dead_code)]
#[path = "../../../crates/capture-core/src/ingest_contract.rs"]
mod ingest_contract;
#[allow(dead_code)]
#[path = "../../../crates/capture-core/src/mapping.rs"]
mod mapping;
#[allow(dead_code)]
#[path = "../../../crates/capture-core/src/profile.rs"]
mod profile;
#[allow(dead_code)]
#[path = "portable_recording.rs"]
mod recording;

#[allow(dead_code)]
#[path = "../../../apps/client/src-tauri/src/platform/windows/catalog.rs"]
mod catalog;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args_os();
    let _program = args.next();
    let directory = args
        .next()
        .map(PathBuf::from)
        .context("missing catalog bundle directory")?;
    ensure!(args.next().is_none(), "unexpected extra arguments");
    catalog::validate_disk_bundle(&directory)
        .with_context(|| format!("catalog bundle {} is not loadable", directory.display()))
}
