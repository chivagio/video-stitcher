//! Build script for the `reco-app` Tauri host crate.
//!
//! Runs `tauri_build::build()` (validates `tauri.conf.json`, generates the
//! Tauri context, and wires codegen) and, like `crates/reco-gui/build.rs`,
//! stamps the short git hash into `GIT_HASH` for build provenance. The gate
//! report (Phase 1 D-04) records the exact build identifier, so keeping this
//! block is deliberate.

fn main() {
    tauri_build::build();

    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !hash.is_empty() {
            println!("cargo:rustc-env=GIT_HASH={hash}");
        }
    }
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
