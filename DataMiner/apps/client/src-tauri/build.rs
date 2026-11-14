fn main() {
    nvidia_optimus::configure_nvidia_optimus_exports(&[
        "dataminer-client",
        "dm-cp2077-raw-demo-record",
    ]);
    println!("cargo:rerun-if-changed=../../../crates/capture-core/optimus-build.rs");
    // Build-script executables can survive a source-directory move in a shared target cache.
    // Read Cargo's CURRENT package directory when the script runs, not when it was compiled.
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("Cargo did not provide the current package directory");
    let config_dir = std::path::Path::new(&manifest_dir)
        .join("../../../config")
        .canonicalize()
        .expect("DataMiner config directory is missing");
    println!(
        "cargo:rustc-env=DATAMINER_CONFIG_DIR={}",
        config_dir.display()
    );
    // The release client embeds catalog JSON with `include_bytes!`. Cargo does
    // not otherwise know that those files are build inputs, so a profile edit
    // could silently leave stale dimensions or hashes in the executable.
    // Cargo resolves this against the current package, including after relocation.
    println!("cargo:rerun-if-changed=../../../config");

    emit_generated_catalog_sources(&config_dir);

    // This is the packaged application execution policy, not the NSIS install
    // mode. Release DataMiner launches must cross Windows UAC. Debug/test PE
    // files retain Tauri's asInvoker manifest so normal Cargo tests never need
    // an interactive elevation prompt.
    const WINDOWS_APP_MANIFEST: &str = include_str!("windows/app.manifest");
    const ACCEPTANCE_HELPER_MANIFEST: &str = include_str!("windows/acceptance-helper.manifest");
    println!("cargo:rerun-if-changed=windows/app.manifest");
    println!("cargo:rerun-if-changed=windows/acceptance-helper.manifest");
    println!("cargo:rerun-if-env-changed=DATAMINER_ACCEPTANCE_HELPER_BUILD");
    println!("cargo:rerun-if-env-changed=DATAMINER_RAW_DEMO_BUILD");
    println!("cargo:rerun-if-env-changed=DATAMINER_BUILD_FLAVOR");
    println!("cargo:rerun-if-env-changed=DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT");
    println!("cargo:rerun-if-env-changed=DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL");
    for name in [
        "DATAMINER_EMBEDDED_ACCOUNT_ENVIRONMENT",
        "DATAMINER_EMBEDDED_ACCOUNT_API_BASE_URL",
    ] {
        if let Ok(value) = std::env::var(name) {
            println!("cargo:rustc-env={name}={value}");
        }
    }

    // The real brand icon, drawn from website/favicon.svg by
    // tools/icon-forge/appicon.py. This used to be a 1x2-pixel placeholder written
    // into OUT_DIR, and — importantly — this script also *overwrote*
    // `bundle.icon` in TAURI_CONFIG to point at it. That override made
    // tauri.conf.json's own `bundle.icon` inert: setting it there had no effect on
    // the shipped binary at all, and the exe carried a single #D97630 pixel while
    // the bundle built clean. If a future placeholder is ever wanted again, change
    // this path — do not reintroduce the config override, because it silently wins
    // over configuration and nothing surfaces the fact.
    println!("cargo:rerun-if-changed=icons/icon.ico");
    let icon = std::path::Path::new("icons/icon.ico");
    assert!(
        icon.exists(),
        "icons/icon.ico is missing; run tools/icon-forge/appicon.py --install <variant>"
    );

    let mut windows_attributes = tauri_build::WindowsAttributes::new().window_icon_path(icon);
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        let manifest = if std::env::var("DATAMINER_ACCEPTANCE_HELPER_BUILD").as_deref() == Ok("1")
            || std::env::var("DATAMINER_RAW_DEMO_BUILD").as_deref() == Ok("1")
        {
            ACCEPTANCE_HELPER_MANIFEST
        } else {
            WINDOWS_APP_MANIFEST
        };
        windows_attributes = windows_attributes.app_manifest(manifest);
    }
    let attributes = tauri_build::Attributes::new().windows_attributes(windows_attributes);
    tauri_build::try_build(attributes).expect("tauri build failed");
}

#[path = "build_support/catalog_gen.rs"]
mod catalog_gen;
use catalog_gen::emit_generated_catalog_sources;

#[path = "../../../crates/capture-core/optimus-build.rs"]
mod nvidia_optimus;
