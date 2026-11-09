//! Helpers shared by integration tests that stage the reference plugin.

use std::path::{Path, PathBuf};

/// Directory of the reference `consumer-quota` plugin.
pub fn example_plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../examples/plugins/consumer-quota")
}

/// Built reference component, if it exists. `OCTRA_TEST_PLUGIN_WASM` overrides
/// the default release path.
pub fn example_wasm() -> Option<PathBuf> {
    let path = std::env::var_os("OCTRA_TEST_PLUGIN_WASM")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            example_plugin_dir().join("target/wasm32-wasip1/release/consumer_quota.wasm")
        });
    path.is_file().then_some(path)
}

/// Copies `plugin.json`, the migrations, and the built component into
/// `destination`, producing a directory accepted by
/// [`crate::APIGatewayWasmPluginFactory::from_artifact`].
pub fn stage_example_artifact(destination: &Path) -> std::io::Result<PathBuf> {
    let example = example_plugin_dir();
    let wasm = example_wasm().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "reference component not built; run `cargo component build --release \
             --manifest-path examples/plugins/consumer-quota/Cargo.toml`",
        )
    })?;
    std::fs::create_dir_all(destination.join("migrations"))?;
    std::fs::copy(example.join("plugin.json"), destination.join("plugin.json"))?;
    std::fs::copy(
        example.join("migrations/000_base.sql"),
        destination.join("migrations/000_base.sql"),
    )?;
    std::fs::copy(wasm, destination.join("plugin.wasm"))?;
    Ok(destination.to_path_buf())
}
