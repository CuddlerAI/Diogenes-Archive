#[path = "../../crates/capture-core/optimus-build.rs"]
mod nvidia_optimus;

fn main() {
    nvidia_optimus::configure_nvidia_optimus_exports(&["capture-lab"]);
    println!("cargo:rerun-if-changed=../../crates/capture-core/optimus-build.rs");
}
