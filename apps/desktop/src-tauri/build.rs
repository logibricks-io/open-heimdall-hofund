//! Build script for the menu-bar package.
//!
//! Registers the window commands so their `allow-*` permissions exist, then
//! embeds the frontend.

/// Runs the Tauri build.
fn main() {
    let attributes =
        tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
            "snapshot",
            "start_gateway",
            "stop_gateway",
            "set_agent",
            "usage_summary",
            "open_config",
        ]));
    if let Err(error) = tauri_build::try_build(attributes) {
        println!("cargo:warning=tauri build failed: {error}");
        std::process::exit(1);
    }
}
