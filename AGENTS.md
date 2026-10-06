# Reco Video Stitcher

Open-source GPU-accelerated panoramic sports camera software.

## Project status

Active release work lives in GitHub issues, PRs, and milestones, not in this
file - check there for the current focus. For the latest stable version and
changelog, see the [GitHub Releases page](https://github.com/reco-project/video-stitcher/releases).
Per-crate consumer pain is logged in each crate's `FRICTION.md`
(e.g. `crates/reco-gui/FRICTION.md`, `crates/reco-obs/FRICTION.md`).

**Rule: document friction, don't work around it.** A reco-core API gap that a
consumer would otherwise hack around gets a `FRICTION.md` entry, not a
consumer-side workaround.

## Architecture (Rust + wgpu)

- `crates/reco-core/` — GPU stitching engine (library crate, no I/O deps)
- `crates/reco-cli/` — CLI binary (`reco stitch`, `reco info`, `reco calibrate`, `reco preview`)
- `crates/reco-io/` — Pluggable I/O backends (FFmpeg decode/encode, GStreamer, libcamera)
- `crates/reco-detect/` — AI detection backends (ORT CPU/GPU, TensorRT, NCNN, CoreML/Metal)
- `crates/reco-autocam/` — AI camera control (directors, trajectory smoothing, ROI filtering)
- `crates/reco-calibrate/` — Stereo camera calibration (AKAZE features, optimization)
- `crates/reco-gui/` — Slint GUI consumer (wgpu zero-copy preview)
- `crates/reco-obs/` — OBS Studio source plugin (async-frame ingestion + BGRA + interactive pan/zoom)

## Key commands

```bash
cargo build                   # Build all crates
cargo test --all              # Run all tests
cargo clippy --all-targets -- -D warnings   # Lint
cargo fmt --all -- --check    # Format check
cargo fmt --all               # Auto-format
cargo doc --no-deps --open    # Generate and open docs
cargo run -p reco-cli -- info # Show GPU info
cargo run -p reco-cli -- stitch left.mp4 right.mp4 -c match.json -o out.mp4
cargo run -p reco-cli -- preview left.mp4 right.mp4 -c match.json
cargo run --release -p reco-cli --features profiling -- stitch left.mp4 right.mp4 -c match.json -o out.mp4 --max-frames 300  # Profile 300 frames → reco-trace.json (open in ui.perfetto.dev)
```

## Headless GUI verification (reco-app)

Agents can run the Tauri app and screenshot it without a human at a display.
The environment ships `Xvfb`, `xdotool`, and ImageMagick `import`; read the
resulting PNG with the image-reading tool.

```bash
cargo build -p reco-app
Xvfb :99 -screen 0 1280x800x24 -nolisten tcp &
export DISPLAY=:99
export RECO_TEST_MEDIA_DIR="$PWD/test-media" RUST_LOG=reco_app=info
dbus-run-session -- target/debug/reco-app &     # window name is "Reco"
xdotool search --name '^Reco$'                   # wait for the window id
import -display :99 -window root /tmp/opencode/shot.png
```

- Existing reference rig: `scripts/phase2-chrome-probe.sh` (Xvfb + `xdotool` +
  log assertions).
- **Native child view covers the webview.** On X11 the Phase 1/2 native
  `wgpu` child view composites ABOVE the webview and hides the top region, and
  pointer input there goes to the child, not the webview. Non-Preview screens
  (Import/Calibrate) are only visible once the native view is suspended
  (Phase 3 plan 03-04) or the presenter is forced to Readback. There is **no
  env override** for presenter selection — it is auto-probed
  (`presenter::choose_presenter`).
- **Wayland fallback:** `weston --backend=headless-backend.so` starts, but
  `weston-screenshooter` aborts in this environment — prefer Xvfb.
- **Image budget (hard limit).** A model request may include **at most 20
  images**; exceeding it fails the whole turn with
  `[invalid_request_error] a request may include at most 20 images`. A UI
  review can easily capture 30+ screenshots, so never read them one by one.
  Capture to disk, then read **contact sheets** built with
  `scripts/contact-sheet.sh` (ImageMagick `montage`), which tiles N frames into
  one labeled image:
  `scripts/contact-sheet.sh -o /tmp/opencode/sheet.png -c 3 -w 640 /tmp/opencode/uat-*.png`.
  Keep at most a couple of full-resolution images (the ones whose fine detail
  matters) in a turn, reference the rest by path in notes, and re-read an
  individual PNG only when a specific detail must be inspected.

## Build & contributing

- Build prerequisites (Rust version, FFmpeg development libraries, clang,
  pkg-config) and the feature-flag matrix: see [README.md](README.md).
- Contribution conventions (branch naming, PR template, CLA): see
  [CONTRIBUTING.md](CONTRIBUTING.md).

## Code standards

- `rustfmt` formatting (config in `rustfmt.toml`)
- `clippy` linting with `-D warnings` (zero warnings policy)
- Doc comments (`///`) on all public items
- Module-level docs (`//!`) explaining purpose
- Tests in each module (`#[cfg(test)] mod tests`)
- All PRs must pass: `cargo fmt --check && cargo clippy && cargo test`
- Clippy must also pass with `--features profiling`
- Keep commit messages and PR descriptions concise and technical (what
  changed + why), especially when written by an AI agent - no filler, no
  marketing tone
- `profiling` feature: opt-in `tracing` + `tracing-chrome` instrumentation (zero-cost when off)

## Context
- Public open-source project (AGPL-3.0) with a growing community and forum
- Users include football clubs, amateur sports teams — prioritize UX clarity
- Open alternative to proprietary sports camera solutions
- Targets: desktop (Win/macOS/Linux), NVIDIA Jetson, cloud, mobile

## When writing code
- Production-grade: handle errors, validate inputs at API boundaries
- Cross-platform (Windows/macOS/Linux) — avoid platform-specific assumptions
- Performance matters: this processes video frames in real time
- Modular: reco-core must be usable as a standalone Rust crate
- Explicit over implicit: no hidden defaults, no magic
- Verify changes actually run - exercise the binary or tests, not just `cargo check`
