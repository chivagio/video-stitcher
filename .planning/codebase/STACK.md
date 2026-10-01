---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
# Technology Stack

**Analysis Date:** 2026-10-01

## Languages

**Primary:**

- Rust 1.92.0 (edition 2024) - Entire workspace: 9 crates plus the out-of-workspace `fuzz/` subcrate. ~71,500 lines across 175 `.rs` files.
- C / C++ - Build-time native glue only: `crates/reco-detect/csrc/tensorrt_wrapper.cpp` (TensorRT wrapper), `crates/reco-obs/src/blog_shim.c` (fixed-arity wrapper around OBS's variadic `blog()` logger).

**Secondary:**

- Slint markup - `crates/reco-gui/ui/main.slint`, compiled at build time by `slint-build`.
- Python 3 - Developer tooling in `scripts/` (`visualize_detections.py`, `gameday.py`, `eval_panner.py`, `field_roi.py`, `gen_trajectory.py`, `plot_roi.py`, `convert-gyroflow-profiles.py`). Uses `numpy`, `scipy`, `cv2` (OpenCV). Not part of the shipped product.
- HTML / JavaScript - `resources/roi_editor.html`, the field-ROI polygon editor opened in an external browser by `reco-gui`.

## Runtime

**Environment:**

- Native Rust binaries — no managed runtime, GC, or VM.
- Binaries produced: `reco` (CLI, `crates/reco-cli`), `reco-gui` (`crates/reco-gui`), and `libreco_obs.so`/`.dll`/`.dylib` as a `cdylib` OBS plugin (`crates/reco-obs`, `crate-type = ["cdylib"]`).
- `crates/reco-core` is a pure library with no I/O dependencies (see `crates/reco-core/README.md`).

**Toolchain:**

- Toolchain pinned to `1.92.0` in `rust-toolchain.toml` (components `rustfmt`, `clippy`, `profile = "minimal"`), because wgpu 28 declares `rust-version = 1.92`.
- Workspace MSRV declared in `Cargo.toml`: `edition = "2024"`, `rust-version = "1.92"`. CI enforces it in the `msrv` job (`.github/workflows/rust.yml`).
- The `fuzz/` subcrate is intentionally excluded from the workspace because `libfuzzer-sys` needs nightly-only sanitizer flags; run with `cargo +nightly fuzz run <target>`.

**Package Manager:**

- Cargo (workspace resolver v3).
- Lockfile: present (`Cargo.lock`, 852 packages locked).
- Python dev scripts use the system interpreter (no `requirements.txt` committed).

## Frameworks

**Core:**

- wgpu 28.0.0 (`naga` 28.0.0) - GPU rendering/compute engine in `crates/reco-core`. Backends: Vulkan (Linux/Android/Jetson), Metal (macOS/iOS), DX12 (Windows), GL (Raspberry Pi V3D, Android fallback). Backend policy lives in `crates/reco-core/src/gpu/mod.rs` (`select_backends`), overridable via `WGPU_BACKEND`.
- Slint ~1.15 (`=1.15.1`) - GUI toolkit in `crates/reco-gui`, built with features `std`, `backend-winit`, `renderer-femtovg-wgpu`, `renderer-software`, `accessibility`, `compat-1-2`, `unstable-wgpu-28`. UI compiled via `slint-build` in `crates/reco-gui/build.rs` (style `fluent-dark`).
- clap 4 (`derive`) - CLI argument parsing in `crates/reco-cli/src/main.rs`.
- winit 0.30 - interactive preview window in `crates/reco-cli/src/preview.rs`.

**I/O Backends (feature-gated):**

- ffmpeg-next 8 / ffmpeg-sys-next 8 - file decode/encode and network streaming in `crates/reco-io/src/ffmpeg/`. Default feature `ffmpeg`.
- GStreamer 0.25 (`gstreamer`, `gstreamer-app`) - live camera ingest in `crates/reco-io/src/gstreamer/`, feature `gstreamer`.
- libcamera - Raspberry Pi CSI capture via the `rpicam-vid` subprocess in `crates/reco-io/src/libcamera.rs`, feature `libcamera`.
- V4L2 - raw MMAP capture in `crates/reco-io/src/v4l2.rs`, feature `v4l2` (uses `libc`).

**Inference (feature-gated):**

- `ort` 2.0.0-rc.12 - ONNX Runtime bindings (`crates/reco-detect/src/ort_session.rs`, `probe.rs`). Default feature `ort`.
- Native TensorRT - `crates/reco-detect/src/detectors/trt/` plus `csrc/tensorrt_wrapper.cpp`, feature `tensorrt-native`; links `nvinfer` + `cudart`.
- NCNN (Tencent) - `crates/reco-detect/src/detectors/ncnn.rs`, feature `ncnn`; links a prebuilt static `ncnn` lib (`NCNN_DIR`).
- CoreML / Metal - `crates/reco-detect/src/coreml_inference.rs`, `metal_compute.rs` (Apple platforms via `objc2-core-ml`, `metal`).

**Testing:**

- Built-in Rust test harness: `#[cfg(test)] mod tests` per module. Run with `cargo test --workspace`.
- Integration tests: `crates/reco-io/tests/{calibration_io.rs,stacked_video_roundtrip.rs}`, `crates/reco-calibrate/tests/{calibrate_videos.rs,regression.rs}`.
- cargo-fuzz / `libfuzzer-sys` 0.4 - three nightly targets in `fuzz/fuzz_targets/` (`calibration_json`, `onnx_names`, `input_path`).
- No `criterion`, `proptest`, `insta`, `assert_cmd`, `mockall`, or `nextest` present. `cargo bench --workspace --no-run` is compiled in CI but no `[[bench]]` targets or benchmark framework are declared.

**Build/Dev:**

- `cc` 1.2 - compiles C/C++ shims in `crates/reco-detect/build.rs` and `crates/reco-obs/build.rs`.
- `bindgen` 0.72 - generates OBS FFI bindings from `libobs` headers at build time (`crates/reco-obs/build.rs`); replaces hand-written FFI.
- `slint-build` 1.15 - Slint UI compilation (`crates/reco-gui/build.rs`).
- `cargo-deny` - supply-chain gates configured by `deny.toml` (advisories / licenses / bans / sources).
- `clippy` (config `clippy.toml`, `too-many-arguments-threshold = 8`) and `rustfmt` (config `rustfmt.toml`, `edition = "2024"`, `max_width = 100`, `use_field_init_shorthand = true`).

## Key Dependencies

**Critical:**

- `wgpu` 28.0.0 - the rendering engine; the pin drives the whole toolchain MSRV.
- `ffmpeg-next` 8.1.0 / `ffmpeg-sys-next` 8.1.0 - the only file/stream I/O path; links against FFmpeg 7.x (n7.1 ABI-compatible).
- `ort` 2.0.0-rc.12 - AI detection runtime; loaded dynamically in release builds (`load-dynamic`).
- `slint` 1.15.1 - desktop GUI.
- `telemetry-parser` 0.3.0 - IMU/GPMF telemetry extraction for calibration, pinned to git rev `2f4218b` (`github.com/AdrianEddy/telemetry-parser`). This is also the source of the git-source allowlist in `deny.toml`.
- `argmin` 0.11 + `argmin-math` 0.5, `nalgebra` 0.35, `ndarray` 0.16, `rayon` 1.12 - calibration optimization and AKAZE feature pipeline in `crates/reco-calibrate`.
- `realfft` 3.5 - FFT for audio cross-correlation sync in calibration.
- `ciborium` 0.2 + `flate2` 1.1 - decompress the bundled Gyroflow lens database (`resources/profiles.cbor.gz`, ~1.5 MB).
- `tracing` 0.1 + `tracing-subscriber` 0.3 (`env-filter`, `json`, `tracing-log`) + `tracing-log` 0.2 - structured logging, bridged to legacy `log` call sites. `tracing-chrome` 0.7 is the opt-in profiling feature.
- `serde` 1 + `serde_json` 1 - calibration files, settings, pipeline events (JSONL), telemetry payloads.
- `thiserror` 2 - library error types; `anyhow` 1 - binary error context in CLI/GUI.

**Infrastructure:**

- `image` 0.25 - PNG/JPEG encode/decode for calibration frames and GUI icons.
- `reqwest` 0.12 (`rustls-tls`, `json`, `blocking`) + `tokio` 1 (`rt`) - GoPro OpenGoPro HTTP client in `crates/reco-control/src/gopro/` (feature `gopro`).
- `ureq` 3.3 - small HTTP client for the GUI GitHub update check and opt-in telemetry (`crates/reco-gui/src/telemetry_client.rs`, `crates/reco-gui/src/main.rs`).
- `directories` 6 - platform config/cache dirs for settings persistence (`crates/reco-io/src/settings.rs`, feature `config`).
- `libloading` 0.9 - dynamic loading of ONNX Runtime (`ort/load-dynamic`), NPP libraries, and platform Vulkan/CUDA libs. Note three `libloading` majors (0.7/0.8/0.9) coexist transitively — flagged in `deny.toml`.
- Platform interop crates: `ash` 0.38 + `libc` (Linux), `windows` 0.62 (DXGI/D3D11/D3D12), `metal` 0.33 + `objc2` 0.6 + `objc2-metal` 0.3 + `objc2-core-ml` 0.3 + `block2` + `foreign-types` (Apple).
- GUI utilities: `rfd` 0.15 (native file dialogs), `arboard` 3 (clipboard for ROI copy/paste), `open` 5 (external browser/forum links), `base64` 0.22, `uuid` 1 (`v4`, telemetry client id).
- `pollster` 0.4 - blocking on wgpu futures; `ctrlc` 3 - graceful Ctrl-C finalization in the CLI.

## Configuration

**Environment:**

- No `.env` files exist in the repo (`.env*` absent). Secrets are not committed.
- Runtime env vars consumed by the code:
  - `RUST_LOG` - log filter (CLI defaults to `info`; `tracing-subscriber` env-filter).
  - `RECO_FFMPEG_LOG` - FFmpeg native log level (`crates/reco-io/src/ffmpeg/mod.rs`).
  - `LIBVA_MESSAGING_LEVEL` - libva diagnostics on Linux (quieted by default in the CLI).
  - `RECO_NO_HWACCEL` - force software decode (set = disable hardware decode).
  - `WGPU_BACKEND` - override GPU backend (`vulkan|dx12|metal|gl`).
  - `ORT_DYLIB_PATH` - explicit ONNX Runtime shared-library path.
  - `RECO_CONFIG_DIR` - override settings directory (`crates/reco-io/src/settings.rs`).
  - `RECO_VRAM_BUDGET_GB` - VRAM budget override in `reco-gui`.
  - `CUDA_HOME`, `CUDA_LIB_DIR`, `TENSORRT_LIB_DIR`, `NCNN_DIR` - native build/link overrides (`crates/reco-detect/build.rs`).
  - `OBS_INCLUDE_DIR` - libobs header location for `reco-obs` bindgen (default `/usr/include/obs`).
  - `FFMPEG_DIR`, `FFMPEG_DLL_DIR` - Windows FFmpeg discovery/bundling in CI.
  - `RECO_AUTOLOAD`, `RECO_AUTOEXPORT`, `RECO_AUTOEXPORT_MODEL`, `RECO_AUTOEXPORT_LOOKAHEAD`, `RECO_AUTOEXPORT_REPEAT` - GUI headless-test hooks gated behind the `automation` feature.
- Settings persistence uses the `directories` crate: config dir under the platform config root, model/engine caches under `{platform_cache_dir}/reco/{trt-cache,coreml-cache}` with `0o700` on Unix (`crates/reco-detect/src/ort_session.rs`).

**Build:**

- Root config: `Cargo.toml` (workspace, resolver 3, release profile `strip = true`, `lto = "thin"`).
- `rust-toolchain.toml` (channel `1.92.0`), `rustfmt.toml`, `clippy.toml`, `deny.toml`.
- `Cargo.lock` committed (852 packages).
- Per-crate `build.rs`: `crates/reco-detect/build.rs`, `crates/reco-gui/build.rs`, `crates/reco-obs/build.rs`.

### Feature flags (matrix)

| Feature | Crate(s) | Purpose |
|---|---|---|
| `ffmpeg` (default) | reco-io | FFmpeg file decode/encode, RTMP/SRT/RTSP |
| `gstreamer` | reco-io, reco-cli | Live camera ingest (Jetson CSI, V4L2) |
| `libcamera` | reco-io, reco-cli | RPi CSI via `rpicam-vid` (stub feature) |
| `v4l2` | reco-io, reco-cli | Raw V4L2 MMAP capture |
| `rpi` | reco-io, reco-cli | FFmpeg `rpi` build hooks |
| `config` | reco-io | User-preference persistence (`directories`) |
| `stacked-output` | reco-io, reco-cli, reco-obs | FFmpeg-backed stacked-video encoder/source |
| `ort` (default) | reco-detect, reco-autocam, reco-cli, reco-gui | ONNX Runtime CPU/CUDA/CoreML/DirectML EPs |
| `cuda` | reco-detect + consumers | ORT CUDA EP |
| `tensorrt` | reco-detect + consumers | ORT TensorRT EP |
| `coreml` | reco-detect + consumers | ORT CoreML EP (macOS) |
| `directml` | reco-detect + reco-gui (Windows) | ORT DirectML EP (DX12 GPUs) |
| `load-dynamic` | reco-detect + consumers | dlopen `libonnxruntime` at runtime (release builds) |
| `tensorrt-native` | reco-detect + consumers | Native TensorRT `.engine` (Jetson, no ORT glibc dep) |
| `ncnn` | reco-detect + consumers | NCNN backend (ARM/RPi5, mobile) |
| `profiling` | workspace | `tracing` + `tracing-chrome` instrumentation, zero-cost when off |
| `keyboard` (default) | reco-control | Keyboard operator transport |
| `gopro` | reco-control, reco-cli | OpenGoPro HTTP camera control |
| `replay` | reco-cli, reco-obs | Stacked-video replay recording |
| `automation` | reco-gui | Headless test preload + auto-export hook (compiled out of release) |

**Consumers must select a detection backend explicitly:** `reco-autocam`, `reco-cli`, and `reco-gui` depend on `reco-detect` with `default-features = false` so the prebuilt `ort-sys` (glibc-sensitive) is not force-linked on Jetson; their own `default` feature re-enables `ort` for standard consumers (`crates/reco-autocam/Cargo.toml`, `crates/reco-cli/Cargo.toml`, `crates/reco-gui/Cargo.toml`).

## Platform Requirements

**Development:**

- Rust 1.92+ (pinned), Cargo.
- FFmpeg development libraries (`libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev libavfilter-dev libswresample-dev`) and `pkg-config clang`.
- GStreamer dev packages (`libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev`) when building `--features gstreamer`.
- `libobs-dev` (Ubuntu) for `reco-obs` bindgen; `OBS_INCLUDE_DIR` overrides the path.
- Python 3 + `numpy`/`scipy`/`cv2` for the `scripts/` dev tools.
- macOS via Homebrew (`brew install ffmpeg pkg-config`); Windows needs FFmpeg 7.x binaries + LLVM/Clang with `FFMPEG_DIR` set.

**Production / deployment targets:**

- Desktop: Linux x86_64/aarch64, macOS x86_64 (Intel) / arm64, Windows x86_64.
- Embedded/edge: NVIDIA Jetson (aarch64 Linux, `nvarguscamerasrc`), Raspberry Pi 5 (V3D GL backend, NCNN), Raspberry Pi CSI (libcamera).
- Cloud workers (Linux) and mobile: Android `aarch64-linux-android` target is compile-checked in CI; iOS uses the Apple Metal/CoreML path. Mobile concrete impls are not yet shipped.
- Distribution: prebuilt CLI + GUI tarballs/zips from `.github/workflows/release.yml`, bundling FFmpeg (Windows), ONNX Runtime 1.24.4, and DirectML 1.15.4; AI models `yolo26n.onnx` / `yolo26n_640.onnx` attached to releases.

**License:** AGPL-3.0-only (`LICENSE`); dual-licensing enabled via the CLA in `CONTRIBUTING.md`. `deny.toml` permits only AGPL-compatible permissive third-party licenses plus explicit per-crate exceptions (project crates, Slint's GPL-3.0 identifier).

---

*Stack analysis: 2026-10-01*
