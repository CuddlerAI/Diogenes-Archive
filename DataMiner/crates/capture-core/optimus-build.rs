// Shared by executable packages that host capture-core. A Rust public/static
// symbol alone does not produce a PE export; the MSVC linker must retain and
// export it from the final executable, including with release LTO enabled.
pub fn configure_nvidia_optimus_exports(binaries: &[&str]) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        // Scope this to video capture hosts. Unrelated tools in the same package
        // may not link capture-core and must not acquire an unresolved export.
        for binary in binaries {
            println!("cargo:rustc-link-arg-bin={binary}=/EXPORT:NvOptimusEnablement,DATA");
            // These are executables, not DLLs consumed through an import library.
            println!("cargo:rustc-link-arg-bin={binary}=/NOIMPLIB");
            println!("cargo:rustc-link-arg-bin={binary}=/NOEXP");
        }
    }
}
