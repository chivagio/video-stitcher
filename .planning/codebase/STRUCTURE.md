---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
# Codebase Structure

**Analysis Date:** 2026-10-01

## Directory Layout

```text
video-stitcher/
├── Cargo.toml               # Workspace manifest: 9 members + excluded fuzz/
├── Cargo.lock               # Locked dependency graph
├── rust-toolchain.toml      # Pinned toolchain (edition 2024, rust-version 1.92)
├── rustfmt.toml             # Formatting config
├── clippy.toml              # Clippy config (zero-warnings policy)
├── deny.toml                # cargo-deny license/advisory policy
├── AGENTS.md / CLAUDE.md    # Agent + contributor project context
├── CONTRIBUTING.md          # Branch/PR/CLA conventions
├── README.md / QUICKSTART.md
├── THIRD_PARTY_NOTICES.md
├── crates/                  # All workspace crates
│   ├── reco-core/           # GPU stitching engine (foundation, no I/O)
│   ├── reco-cli/            # `reco` binary
│   ├── reco-io/             # FFmpeg/GStreamer/libcamera I/O + StitchJob
│   ├── reco-detect/         # AI detector backends
│   ├── reco-autocam/        # Trackers + panners + ROI filter
│   ├── reco-calibrate/      # Stereo calibration (AKAZE + optimizer)
│   ├── reco-control/        # Transport-agnostic control vocabulary
│   ├── reco-gui/            # Slint desktop app
│   └── reco-obs/            # OBS Studio source plugin (cdylib)
├── fuzz/                    # Excluded subcrate (nightly libfuzzer targets)
│   └── fuzz_targets/        # calibration_json.rs, input_path.rs, onnx_names.rs
├── resources/               # Bundled data: profiles.cbor.gz, roi_editor.html
├── scripts/                 # Python tooling (profile conversion, panner eval, ROI)
├── .github/workflows/       # CI: rust.yml, build-test.yml, test-build-gui.yml, release.yml, cla.yml
└── .planning/codebase/      # This analysis output
```

## Directory Purposes

**`crates/`** — every workspace member. Each crate owns a single architectural
responsibility; the dependency direction is always toward `reco-core`.

**`crates/reco-core/src/`:**

- Purpose: pure stitching engine. The only crate with no workspace dependencies.
- Contains:
  - `lib.rs` — crate root, `wgpu` re-export, `profile_scope!` macro, module list.
  - `core/` — `StitchCore`, push-first canonical engine (`mod.rs`, `pose.rs`,
    `render.rs`, `replay_buffer.rs`, `replay_management.rs`, `types.rs`).
  - `session/` — `StitchSession` batch/pull adapter (`mod.rs`, `run_loop.rs`,
    `frame_processing.rs`, `detection.rs`, `detection_dispatch.rs`,
    `frame_buffer.rs`, `vram_pool.rs`, `wiring.rs`, `types.rs`, `tests.rs`,
    `zero_copy_linux.rs`).
  - `render/` — wgpu pipeline (`pipeline.rs`, `renderer.rs`, `stitch_renderer.rs`,
    `scene.rs`, `viewport.rs`, `planes.rs`).
  - `projection/` — coordinate mapping (`mod.rs`, `coverage.rs`, `geometry.rs`,
    `virtual_camera.rs`).
  - `gpu/` — device + format plumbing (`mod.rs`, `nv12_converter.rs`,
    `rgba_readback.rs`, `color_grade.rs`, `yuv_stack_packer.rs`).
  - `interop/` — platform zero-copy bridges (`cuda.rs`, `vulkan.rs`, `dmabuf.rs`,
    `metal.rs`, `d3d11.rs`, `zero_copy.rs`).
  - `detect/` — trait vocabulary (`detector.rs`, `director.rs`, `panner.rs`,
    `tracker.rs`, `pipeline_event.rs`).
  - `lens/` — fisheye model (`mod.rs`, `undistort.rs`, `preview.rs`,
    `rig_correction.rs`).
  - `shaders/` — WGSL sources (`fisheye.wgsl`, `bayer_demosaic.wgsl`,
    `color_grade.wgsl`, `rgba_to_nv12.wgsl`, `yuv420p_stack_pack.wgsl`,
    `cylindrical_mono.wgsl`).
  - Flat modules: `calibration.rs`, `source.rs`, `encoder.rs`, `telemetry.rs`,
    `bayer.rs`, `async_encode.rs`, `nvbuf_transform.rs`.

**`crates/reco-cli/src/`:**

- Purpose: the `reco` binary, one file per subcommand.
- Contains: `main.rs` (clap definitions + dispatch), `stitch.rs`, `preview.rs`,
  `camera.rs`, `libcamera_cmd.rs`, `calibrate.rs`, `helpers.rs`.

**`crates/reco-io/src/`:**

- Purpose: concrete codec sources/encoders + the one-shot job API.
- Contains: `lib.rs` (feature-gated module list), `adapters.rs` (trait bridges),
  `ffmpeg/` (`decoder.rs`, `encoder.rs`, `calibration_io.rs`, `hw_upload.rs`),
  `gstreamer/` (`camera.rs`, `nvmm.rs`), `stacked_video/` (`mod.rs`, `encoder.rs`,
  `source.rs`, `replay.rs`), `output.rs`, `settings.rs`, `smart_source.rs`,
  `stitch_job.rs`, `zero_copy.rs`, `jsonl_sink.rs`, `v4l2.rs`, `libcamera.rs`.

**`crates/reco-detect/src/`:**

- Purpose: all AI inference backends and GPU preprocess primitives.
- Contains: `lib.rs`, `detectors/` (`cpu.rs`, `ort_gpu.rs`, `metal.rs`, `ncnn.rs`,
  `mod.rs`, `trt/{mod,engine,cuda,sys}.rs`), `ort_session.rs`, `probe.rs`,
  `wgpu_preprocess.rs`, `cuda_kernels.rs`, `npp_interop.rs`, `coreml_inference.rs`,
  `metal_compute.rs`; C++ wrapper `csrc/tensorrt_wrapper.cpp` + `build.rs`.

**`crates/reco-autocam/src/`:**

- Purpose: the intelligence layer — tracking and camera-motion policies.
- Contains: `lib.rs` (`AutocamConfig`, `setup_autocam`), `trackers/`
  (`ball.rs`, `class_provider.rs`, `filters/`), `panners/` (`field.rs`,
  `sweep.rs`, `file_panner.rs`), `roi_filter.rs`, `tracking_mode.rs`,
  `wgpu_detector.rs`.

**`crates/reco-calibrate/src/`:**

- Purpose: stereo calibration pipeline.
- Contains: `lib.rs`, `akaze/` (feature detector internals), `pipeline.rs`,
  `optimizer.rs`, `features.rs`, `filter.rs`, `ransac.rs`, `geometry.rs`,
  `sampling.rs`, `traits.rs`, `defaults.rs`, `types.rs`, `lens_database.rs`,
  `audio_sync.rs`, `telemetry.rs`, `live.rs`, `video.rs`, `error.rs`.

**`crates/reco-control/src/`:**

- Purpose: a single control vocabulary shared by consumers.
- Contains: `lib.rs` (`ControlIntent`), `pose_control.rs`, `intent_translator.rs`,
  `keyboard.rs`, `gopro/` (`mod.rs`, `http.rs`, `status.rs`, `constants.rs`, `error.rs`).

**`crates/reco-gui/src/`:**

- Purpose: Slint desktop application.
- Contains: `main.rs` (4.7k-line UI controller), `preview.rs` (`PreviewBridge`
  zero-copy), `playback.rs`, `export.rs`, `settings.rs`, `telemetry_client.rs`,
  `toast.rs`; markup at `crates/reco-gui/ui/main.slint`.

**`crates/reco-obs/src/`:**

- Purpose: OBS plugin cdylib.
- Contains: `lib.rs` (module registration, `ffi_catch!`), `source.rs`
  (`obs_source_info` callbacks), `ffi.rs` (bindgen-generated), `obs_log.rs`,
  `blog_shim.c`; `build.rs` runs bindgen + compiles the C shim.

**`fuzz/`** — workspace-excluded nightly fuzz subcrate. Targets mirror the parsers
at trust boundaries: `calibration_json.rs`, `input_path.rs`, `onnx_names.rs`.

**`resources/`** — `profiles.cbor.gz` (Gyroflow lens-profile database consumed by
`crates/reco-calibrate/src/lens_database.rs`) and `roi_editor.html` (browser tool for
authoring the field ROI polygon used by `crates/reco-autocam/src/roi_filter.rs`).

**`scripts/`** — Python developer tooling only, not shipped:
`convert-gyroflow-profiles.py`, `eval_panner.py`, `field_roi.py`, `gameday.py`,
`gen_trajectory.py`, `plot_roi.py`, `visualize_detections.py`.

## Key File Locations

**Entry Points:**

- `crates/reco-cli/src/main.rs`: `reco` binary `main()` + clap subcommands.
- `crates/reco-gui/src/main.rs`: Slint app entry, wgpu 28 backend selection.
- `crates/reco-obs/src/lib.rs`: `obs_module_load` / `obs_module_set_pointer` FFI exports.
- `crates/reco-io/src/stitch_job.rs`: `StitchJob` one-shot file→file orchestration.

**Configuration:**

- `Cargo.toml`: workspace members, shared edition/rust-version, workspace deps.
- `crates/*/Cargo.toml`: per-crate features and platform dependency gates.
- `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `deny.toml`: toolchain/lint policy.
- `.github/workflows/rust.yml`: check/lint, test, doc, cargo-deny, audit, MSRV,
  GPU-backend compile checks, bench build, macOS check.

**Core Logic:**

- `crates/reco-core/src/core/mod.rs`: `StitchCore` — the canonical engine.
- `crates/reco-core/src/session/mod.rs`: `StitchSession` — batch/pull adapter.
- `crates/reco-core/src/render/pipeline.rs`: `StitchPipeline`.
- `crates/reco-core/src/render/renderer.rs`: wgpu renderer + shader uniforms.
- `crates/reco-core/src/gpu/mod.rs`: `GpuContext`, `OutputFormat`.
- `crates/reco-core/src/detect/mod.rs`: `DetectionTarget` trait + module list.
- `crates/reco-core/src/projection/mod.rs`: `Projection` trait, coordinate math.
- `crates/reco-core/src/calibration.rs`: `MatchCalibration`/`CameraParams` parsing.

**Testing:**

- Unit tests are co-located via `#[cfg(test)] mod tests` in 63 source files
  (`reco-core` 26, `reco-calibrate` 9, `reco-io` 7, `reco-autocam` 7,
  `reco-detect` 5, `reco-control` 4, `reco-gui` 3, `reco-obs` 2). `reco-cli` has none.
- Integration tests: `crates/reco-calibrate/tests/{calibrate_videos.rs,regression.rs}`
  (+ `tests/data/*.json` fixtures), `crates/reco-io/tests/{calibration_io.rs,stacked_video_roundtrip.rs}`.
- Examples: `crates/reco-calibrate/examples/{dump_points,dump_undistorted,optimize_points}.rs`,
  `crates/reco-cli/examples/vram_leak_repro.rs`.
- Fuzz: `fuzz/fuzz_targets/{calibration_json,input_path,onnx_names}.rs`.

## Naming Conventions

**Files:**

- `snake_case.rs` everywhere. Multi-word modules use a directory with a `mod.rs`
  when they have submodules (`session/`, `render/`, `detect/`); single modules are
  flat files (`calibration.rs`, `source.rs`).
- Platform variants are named by API, not OS: `cuda.rs`, `vulkan.rs`, `dmabuf.rs`,
  `metal.rs`, `d3d11.rs` — the `#[cfg]` gate lives in the parent `mod.rs`.
- FFI shims use explicit names: `ffi.rs`, `blog_shim.c`, `tensorrt_wrapper.cpp`.

**Crates:**

- Workspace crates are `reco-<domain>` (`reco-core`, `reco-io`, `reco-detect`,
  `reco-autocam`, `reco-calibrate`, `reco-control`, `reco-gui`, `reco-obs`,
  `reco-cli`). In Rust code they are referred to as `reco_core`, `reco_io`, etc.

**Rust identifiers:**

- Types: `UpperCamelCase` (`StitchCore`, `ViewportPosition`, `UnifiedDetector`).
- Traits: noun-phrase, no `I` prefix (`Projection`, `Panner`, `Tracker`, `Encoder`).
- Enums variants: `UpperCamelCase`; platform frame variants are named
  `GpuResident`, `MetalResident`, `D3d11Resident` (`crates/reco-core/src/source.rs`).
- Functions/methods: `snake_case`, verb-first (`submit_frame_yuv`, `configure_gpu_source`,
  `safe_clamp`, `setup_autocam`).
- Errors: `<Domain>Error` (`GpuError`, `RenderError`, `SessionError`, `SourceError`).
- Feature flags: lowercase, backend/capability named (`profiling`, `tensorrt-native`,
  `stacked-output`, `automation`, `replay`, `gopro`, `libcamera`, `v4l2`).
- `#[cfg]` attributes are the platform boundary; there is no runtime OS `match`
  for module selection.

**C string constants (OBS):** `PROP_*` for property keys, `SOURCE_*` for identity
(`crates/reco-obs/src/source.rs:46`).

## Where to Add New Code

**New GPU render stage / shader effect:**

- WGSL: `crates/reco-core/src/shaders/<name>.wgsl`.
- GPU plumbing: `crates/reco-core/src/gpu/<name>.rs`, re-export from
  `crates/reco-core/src/gpu/mod.rs`.
- Pipeline wiring: `crates/reco-core/src/render/pipeline.rs` or `renderer.rs`.

**New panoramic projection:**

- Implement `Projection` in `crates/reco-core/src/projection/mod.rs` (or a new
  submodule registered there); add its WGSL to `shaders/`; note that `camera_count()`
  must match the `CameraInput` used. Keep the L-shape default intact.

**New frame source or encoder backend:**

- Implement `FrameSource`/`Encoder` in `crates/reco-io/src/<backend>/`, keep backend
  code trait-free, and add the trait bridge to `crates/reco-io/src/adapters.rs`.
- Register the module behind a feature flag in `crates/reco-io/src/lib.rs`.

**New detector backend:**

- Add under `crates/reco-detect/src/detectors/`, implement `UnifiedDetector`,
  gate with a Cargo feature in `crates/reco-detect/Cargo.toml`, and re-export from
  `crates/reco-detect/src/lib.rs`. Re-export through
  `crates/reco-autocam/src/lib.rs` if consumer-facing.

**New tracker or panner:**

- `crates/reco-autocam/src/trackers/<name>.rs` or
  `crates/reco-autocam/src/panners/<name>.rs`; implement the `reco-core` trait;
  wire into `crates/reco-autocam/src/tracking_mode.rs` + `setup_autocam`.

**New CLI subcommand:**

- Add a variant to `enum Commands` in `crates/reco-cli/src/main.rs`, create/extend a
  handler module (`<name>.rs`), delegate from `main()`.

**New control transport:**

- Add to `crates/reco-control/` behind a feature flag; translate native events into
  `ControlIntent` and let `IntentTranslator` dispatch.

**New feature:**

- Primary engine code: `crates/reco-core/src/core/` (push) and
  `crates/reco-core/src/session/` (batch).
- Orchestration: `crates/reco-io/src/stitch_job.rs`.
- Tests: co-located `#[cfg(test)] mod tests` for units; add
  `crates/<crate>/tests/*.rs` for integration tests.

**Utilities:**

- Shared helpers belong in the lowest crate that can own them. Cross-crate utilities
  go in `reco-core` only if they are I/O-free and domain-generic; otherwise in
  `reco-io` (I/O-adjacent) or the consumer.

## Special Directories

**`crates/reco-core/src/shaders/`:**

- Purpose: WGSL shader sources embedded with `include_str!` at compile time.
- Generated: No (authored).
- Committed: Yes.

**`crates/reco-obs/src/ffi.rs`:**

- Purpose: OBS FFI bindings.
- Generated: Yes — by `bindgen` in `crates/reco-obs/build.rs` against installed libobs headers.
- Committed: Yes (checked in; regenerate on OBS API bump).

**`crates/reco-detect/csrc/`:**

- Purpose: C++ TensorRT wrapper compiled by `build.rs` (`links = "tensorrt_wrapper"`).
- Generated: No (authored C++).
- Committed: Yes.

**`resources/`:**

- Purpose: bundled runtime data (`profiles.cbor.gz`, `roi_editor.html`).
- Generated: No.
- Committed: Yes.

**`fuzz/`:**

- Purpose: nightly-only fuzz subcrate, excluded from the workspace.
- Generated: No.
- Committed: Yes (targets + `.gitignore` for corpus/target).

**`.planning/codebase/`:**

- Purpose: GSD codebase-map documents (this file).
- Generated: Yes — by `/gsd-map-codebase`.
- Committed: Yes (planning artifacts).

---

*Structure analysis: 2026-10-01*
