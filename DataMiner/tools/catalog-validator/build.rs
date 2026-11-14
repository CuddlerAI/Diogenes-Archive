// 本 crate 以 #[path] 挂载客户端的 catalog.rs（见 src/main.rs），后者在
// dev-catalog 特性下 include! OUT_DIR 里由 shipped-games.json 生成的内嵌
// 清单，因此这里必须复用客户端同一个生成器——否则 dev-catalog 特性是
// 一个声明了却编译不了的死配置。
#[path = "../../apps/client/src-tauri/build_support/catalog_gen.rs"]
mod catalog_gen;

fn main() {
    let config_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config")
        .canonicalize()
        .expect("DataMiner config directory is missing");
    println!(
        "cargo:rustc-env=DATAMINER_CONFIG_DIR={}",
        config_dir.display()
    );
    println!("cargo:rerun-if-changed={}", config_dir.display());
    catalog_gen::emit_generated_catalog_sources(&config_dir);
}
