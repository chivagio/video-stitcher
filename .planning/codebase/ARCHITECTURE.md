---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
<!-- refreshed: 2026-10-01 -->

# Architecture

**Analysis Date:** 2026-10-01

## System Overview

Reco is a Cargo workspace (`Cargo.toml`, resolver `3`, edition 2024) of nine
library/binary crates plus an excluded `fuzz/` subcrate. The architecture is a
strict dependency DAG rooted at `reco-core`, the pure GPU stitching engine with
no I/O dependencies. Everything that touches a file, camera, AI runtime, or host
application is layered above it.

```text
┌───────────────────────────────────────────────────────────────────────────┐
│                         Consumer / Application Layer                       │
├──────────────────────┬─────────────────────┬──────────────────────────────┤
│   reco-cli           │   reco-gui          │   reco-obs                   │
│  `reco-cli/src/main` │ `reco-gui/src/main` │ `reco-obs/src/lib.rs`        │
│  (clap subcommands)  │ (Slint UI + wgpu)   │ (OBS csrc cdylib plugin)     │
└──────────┬───────────┴──────────┬──────────┴──────────────┬───────────────┘
           │                      │                         │
           ▼                      ▼                         ▼
┌───────────────────────────────────────────────────────────────────────────┐
│                        Orchestration / Engine Layer                        │
│   reco-io `StitchJob` (one-shot file→file)   `StitchCore` / `StitchSession`│
│   `reco-io/src/stitch_job.rs`                 `reco-core/src/core/mod.rs`   │
│                                               `reco-core/src/session/mod.rs`│
└──────────┬──────────────────────────────────────────────┬─────────────────┘
           │                                              │
           ▼                                              ▼
┌──────────────────────────────┐        ┌───────────────────────────────────┐
│      Domain / AI Layer       │        │     I/O Backend Layer             │
│  reco-autocam (trackers,     │        │  reco-io: ffmpeg, gstreamer,      │
│   panners, ROI filter)       │        │   libcamera, v4l2, stacked_video  │
│  reco-detect (ORT/TRT/NCNN/  │        │ reco-calibrate: AKAZE, optimizer  │
│   CoreML/Metal detectors)    │        │ reco-control: intent vocabulary   │
└──────────────┬───────────────┘        └────────────────┬──────────────────┘
               │                                         │
               └──────────────────┬──────────────────────┘
                                  ▼
┌───────────────────────────────────────────────────────────────────────────┐
│                      reco-core (foundation, no I/O)                        │
│  GPU pipeline · projection · lens model · calibration · GPU interop ·      │
│  detection traits · session engine · telemetry                             │
│  `reco-core/src/lib.rs`                                                    │
└───────────────────────────────────────────────────────────────────────────┘
```

### Crate Dependency Graph (verified from each `crates/*/Cargo.toml`)

```text
reco-core        ← (foundation; depends on no workspace crate)
  ▲
  ├── reco-control     (control vocabulary; wgpu-free)
  ├── reco-detect      (AI backends; reuses reco-core interop/CUDA FFI)
  ├── reco-io          (frame sources + encoders + stacked video)
  │
  ├── reco-autocam     → reco-core, reco-detect
  ├── reco-calibrate   → reco-core, reco-io (optional `io` feature)
  │
  ├── reco-cli         → reco-core, reco-control, reco-io, reco-autocam(opt), reco-calibrate
  ├── reco-gui         → reco-core, reco-control, reco-io, reco-calibrate, reco-detect, reco-autocam(opt)
  └── reco-obs         → reco-core, reco-control, reco-io(opt `replay`)

fuzz/  (excluded from workspace; nightly libfuzzer)
  └── targets: calibration_json (reco-calibrate), input_path (reco-core), onnx_names (reco-detect)
```

`reco-core` is import-clean of every sibling crate (verified: no `reco-*` path
dependencies in `crates/reco-core/Cargo.toml`). This is the enforced boundary
that keeps it usable as a standalone crate (see `AGENTS.md`).

## Component Responsibilities

| Component | Responsibility | File |
|-----------|----------------|------|
| **reco-core** | GPU stitching engine, projection/lens/calibration, detection *traits*, session engine | `crates/reco-core/src/lib.rs` |
| `core` (StitchCore) | Push-first canonical engine: owns pipeline, readback, coverage, tracker/panner, replay buffer | `crates/reco-core/src/core/mod.rs` |
| `session` (StitchSession) | Pull/batch adapter: NV12 conversion, async encode, lookahead, legacy detection pipeline | `crates/reco-core/src/session/mod.rs` |
| `render` | wgpu pipelines, scene geometry, viewport crop, surface renderer | `crates/reco-core/src/render/mod.rs` |
| `projection` | Camera-pixel ↔ panorama coordinate mapping; `Projection` trait | `crates/reco-core/src/projection/mod.rs` |
| `interop` | Zero-copy platform bridges (CUDA/Vulkan, DMA-BUF, Metal, D3D11) | `crates/reco-core/src/interop/mod.rs` |
| `gpu` | Device init (`GpuContext`), NV12 converter, RGBA readback, color grade, YUV stack packer | `crates/reco-core/src/gpu/mod.rs` |
| `detect` | Trait vocabulary: `UnifiedDetector`, `Tracker`, `Panner`, `ViewportPosition`, event sink | `crates/reco-core/src/detect/mod.rs` |
| **reco-io** | Frame sources + encoders; one-shot `StitchJob`; stacked-video pack/unpack | `crates/reco-io/src/lib.rs` |
| **reco-detect** | YOLO detector backends (ORT CPU/GPU, TensorRT native, NCNN, CoreML/Metal) + preprocess | `crates/reco-detect/src/lib.rs` |
| **reco-autocam** | Trackers (`BallTracker`, `ClassProvider`), panners (`FieldPanner`, `SweepPanner`, `FilePanner`), ROI filter | `crates/reco-autocam/src/lib.rs` |
| **reco-calibrate** | AKAZE features, matching/RANSAC, Nelder-Mead optimizer, telemetry sync, lens DB | `crates/reco-calibrate/src/lib.rs` |
| **reco-control** | Transport-agnostic `ControlIntent` vocabulary + `PoseControl` state machine | `crates/reco-control/src/lib.rs` |
| **reco-cli** | `reco` binary: stitch/preview/camera/libcamera/calibrate/info/gopro subcommands | `crates/reco-cli/src/main.rs` |
| **reco-gui** | Slint desktop app; zero-copy wgpu preview sharing Slint's device | `crates/reco-gui/src/main.rs` |
| **reco-obs** | OBS source plugin cdylib (`obs_source_info` callbacks, bindgen FFI) | `crates/reco-obs/src/lib.rs`, `crates/reco-obs/src/source.rs` |

## Pattern Overview

**Overall:** Layered library + pluggable-trait (dependency-inversion) architecture.

**Key Characteristics:**

- **Trait seams at the foundation.** `reco-core` defines traits (`FrameSource`,
  `Encoder`, `UnifiedDetector`, `Tracker`, `Panner`, `Projection`, `CameraInput`,
  `PipelineEventSink`); implementations live in outer crates (`reco-io`,
  `reco-detect`, `reco-autocam`). Consumers program against the traits.
- **Two-level engine API.** `StitchCore` is push-first (live sports); `StitchSession`
  is a pull/batch adapter layered on top. Both implement `DetectionTarget`
  (`crates/reco-core/src/detect/mod.rs:17`) so `reco_autocam::setup_autocam`
  configures either.
- **Platform-specific code is `#[cfg]`-gated at the module level**, never with
  runtime branching where avoidable — e.g. `interop/mod.rs` exposes `cuda`/`d3d11`/
  `dmabuf`/`metal`/`vulkan` per target OS (`crates/reco-core/src/interop/mod.rs:10`).
- **Zero-copy is a first-class path, not an optimization.** `StereoFrame` has
  `GpuResident` / `MetalResident` / `D3d11Resident` variants consumed directly by
  the renderer and detector (`crates/reco-core/src/session/frame_processing.rs:108`).
- **Errors are typed per crate** with `thiserror`; no stringly-typed error channels
  at API boundaries. `Send + Sync` on error enums so they cross worker threads.

## Layers

**Foundation Layer (`reco-core`):**

- Purpose: GPU rendering + geometry + trait contracts. No filesystem, no codec, no AI runtime.
- Location: `crates/reco-core/src/`
- Contains: `GpuContext`, `StitchPipeline`, `StitchCore`, `StitchSession`,
  `Projection`/`CameraInput`/`UnifiedDetector`/`Tracker`/`Panner` traits,
  lens model, calibration parsing, telemetry.
- Depends on: `wgpu`, `nalgebra`, `bytemuck`, `pollster`, `serde`, plus
  platform crates (`ash`, `windows`, `metal`/`objc2-*`).
- Used by: every other crate.

**Domain / AI Layer:**

- Purpose: turn detections into camera motion and features into calibration.
- Location: `crates/reco-detect/src/`, `crates/reco-autocam/src/`, `crates/reco-calibrate/src/`.
- Contains: detector backends, trackers/panners, AKAZE + optimizer pipeline.
- Depends on: `reco-core` (and `reco-detect` for autocam, `reco-io` optionally for calibrate).
- Used by: `reco-cli`, `reco-gui`.

**I/O Backend Layer (`reco-io`):**

- Purpose: implement `FrameSource`/`Encoder` over concrete codecs and protocols.
- Location: `crates/reco-io/src/`
- Contains: `ffmpeg/`, `gstreamer/`, `libcamera.rs`, `v4l2.rs`, `smart_source.rs`,
  `stitch_job.rs`, `stacked_video/`, `output.rs`, `settings.rs`, `zero_copy.rs`.
- Depends on: `reco-core`.
- Used by: `reco-cli`, `reco-gui`, `reco-obs`, `reco-calibrate`.

**Orchestration Layer:**

- Purpose: compose GPU + I/O + AI into a runnable job.
- Location: `crates/reco-io/src/stitch_job.rs` (`StitchJob`, "Layer 3 API"),
  `crates/reco-core/src/core/mod.rs` + `session/` (engine).
- Depends on: all lower layers.

**Application Layer:**

- Purpose: human-facing binaries / host plugin.
- Location: `crates/reco-cli/src/main.rs`, `crates/reco-gui/src/main.rs`,
  `crates/reco-obs/src/lib.rs`.

## Data Flow

### Primary Stitch Flow (batch file → file)

1. CLI parses `reco stitch` and calls `stitch::run_stitch` (`crates/reco-cli/src/main.rs:779`, `crates/reco-cli/src/stitch.rs`).
2. `StitchJob::run` opens `SmartFileSource`, which probes input and selects a decode
   path (CUDA/Vulkan zero-copy on Linux, D3D11VA on Windows, VideoToolbox/Metal on
   macOS, else CPU FFmpeg) (`crates/reco-io/src/smart_source.rs`, `crates/reco-io/src/stitch_job.rs`).
3. `StitchSession::run` drives the frame loop; if lookahead > 0 it uses
   `run_buffered`, else `run_immediate` (`crates/reco-core/src/session/run_loop.rs:150`).
4. Per frame, `process_frame_any` dispatches detection → pose → render → replay →
   telemetry (`crates/reco-core/src/session/frame_processing.rs:90`).
5. `StitchPipeline` uploads YUV420P/NV12 planes as textures and runs the fragment
   shader (`fisheye.wgsl`) doing YUV→RGB (BT.709) + KB4 fisheye undistortion +
   L-shape composite → RGBA render target (`crates/reco-core/src/render/renderer.rs:1`).
6. Optional color-grade pass (`crates/reco-core/src/gpu/color_grade.rs`).
7. `Nv12Converter` compute-converts RGBA → NV12 on the GPU and reads back the
   smaller buffer (`crates/reco-core/src/gpu/nv12_converter.rs`).
8. Frames fan out to `AsyncEncodeThread` (primary + extra encoders) and are
   finalized on `StitchSession::finish` (`crates/reco-core/src/session/mod.rs:409`).

### Detection → Camera-Motion Flow

```text
Raw camera frames (pre-stitch)
  → UnifiedDetector (reco-detect: CpuYolo / OrtGpu / TrtGpu / Ncnn / Metal)
  → Vec<Detection> (normalized camera coords)          `reco-core/src/detect/detector.rs`
  → camera_to_panorama mapping
  → MappedDetection (panorama yaw/pitch)               `reco-core/src/detect/director.rs`
  → Tracker(s) → TrackedEntity + WorldState            `reco-core/src/detect/tracker.rs`
        (impls: reco-autocam BallTracker / ClassProvider)
  → Panner::decide / decide_with_lookahead             `reco-core/src/detect/panner.rs`
        (impls: reco-autocam FieldPanner / SweepPanner / FilePanner)
  → ViewportPosition (raw, unclamped)
  → CoverageBoundary::safe_clamp + rig-correction      `reco-core/src/projection/coverage.rs`
  → StitchPipeline::set_fov / render
  → PipelineEventSink (JSONL)                          `crates/reco-io/src/jsonl_sink.rs`
```

Autocam wiring entry point: `reco_autocam::setup_autocam(&mut DetectionTarget, &AutocamConfig, fps, field_roi)`
(`crates/reco-autocam/src/lib.rs:192`), consumed by CLI stitch and GUI export.

### Calibration Flow

1. Frame pairs → GPU undistort (`reco_core::lens::undistort::GpuUndistort`).
2. AKAZE feature detect + descriptor match (`crates/reco-calibrate/src/akaze/`,
   `features.rs`).
3. Spatial + RANSAC filter (`crates/reco-calibrate/src/filter.rs`, `ransac.rs`).
4. Nelder-Mead optimizer minimizes reprojection error → `PlaneLayout`
   (`crates/reco-calibrate/src/optimizer.rs`).
5. Result serialized to `MatchCalibration` JSON consumed by the render pipeline
   (`crates/reco-core/src/calibration.rs`).

Stages are trait-driven: `FeatureDetector`, `FeatureMatcher`, `PointFilter`,
`CostFunction` (`crates/reco-calibrate/src/traits.rs`), with defaults in
`defaults.rs`.

### GPU Zero-Copy Flow (per platform)

| Platform | Path | Files |
|----------|------|-------|
| Linux | NVDEC → CUDA/Vulkan shared memory → wgpu texture | `crates/reco-core/src/interop/cuda.rs`, `vulkan.rs`, `crates/reco-io/src/zero_copy.rs` |
| Linux (Jetson) | NVMM NvBufSurface → DMA-BUF import | `crates/reco-core/src/interop/dmabuf.rs`, `crates/reco-core/src/nvbuf_transform.rs` |
| Windows | D3D11VA decoded NV12 → staging textures | `crates/reco-core/src/interop/d3d11.rs`, `crates/reco-core/src/session/frame_processing.rs:185` |
| macOS/iOS | VideoToolbox CVPixelBuffer → Metal texture cache | `crates/reco-core/src/interop/metal.rs`, `crates/reco-core/src/session/mod.rs:166` |

**State Management:**

- Per-session state lives inside `StitchCore` / `StitchSession` (`frame_count`,
  `previous_panner_pose`, `vram_pool`, `lookahead_world_states`).
- GPU resources (textures, bind groups, pipelines) are owned by `Renderer` inside
  `StitchPipeline`; `GpuContext` is a cheap clone (Arc-backed wgpu handles).
- The only process-global mutable state is in the OBS plugin
  (`OBS_RECORDING_OR_STREAMING`, `MODULE_PTR` atomics — `crates/reco-obs/src/lib.rs:51`).

## Key Abstractions

**`GpuContext`:**

- Purpose: single shared wgpu device/queue/adapter for all pipeline stages.
- Examples: `crates/reco-core/src/gpu/mod.rs:103`; windowed variant built by
  `GpuContext::from_device_queue` in `crates/reco-gui/src/preview.rs:68`.
- Pattern: Arc-backed clone; `OutputFormat` enum wraps the used texture formats so
  headless consumers avoid depending on wgpu directly (re-exported from
  `crates/reco-core/src/lib.rs:74`).

**`StitchPipeline`:**

- Purpose: owns scene geometry + renderer, renders YUV/NV12 pairs to RGBA/NV12.
- Examples: `crates/reco-core/src/render/pipeline.rs:68`.
- Pattern: created `with_gpu`; `configure_gpu_source` builds zero-copy bind groups
  (`crates/reco-core/src/render/pipeline.rs:88`).

**`StitchCore` (canonical push-first):**

- Purpose: `submit_frame_yuv` / `submit_frame_bgra` / `..._at_pose`, owns coverage,
  replay ring, trackers/panners, stacked-replay recorder.
- Examples: `crates/reco-core/src/core/mod.rs:85`, submodules `render.rs`, `pose.rs`,
  `replay_buffer.rs`, `replay_management.rs`.
- Pattern: push API for live; `StitchSession` layers pull/batch on top.

**`Projection` trait:**

- Purpose: make the L-shape geometry one of N possible panoramic projections.
- Examples: `LShapeProjection` (`crates/reco-core/src/projection/mod.rs:96`),
  `CylindricalProjection` (`crates/reco-core/src/projection/mod.rs`, shader
  `crates/reco-core/src/shaders/cylindrical_mono.wgsl`).
- Pattern: `Box<dyn Projection>` slot in `StitchCoreConfig`; `camera_count()` is
  validated against `CameraInput` at construction (`crates/reco-core/src/core/mod.rs:191`).

**`UnifiedDetector` / `Tracker` / `Panner`:**

- Purpose: the detector→tracker→panner→pose chain, each independently swappable.
- Examples: trait defs in `crates/reco-core/src/detect/{detector,tracker,panner}.rs`;
  impls in `crates/reco-detect/src/detectors/` and `crates/reco-autocam/src/{trackers,panners}/`.
- Pattern: implementations are stateless-by-class or stateful-singleton; panner
  output is deliberately unclamped and the session applies coverage clamping.

**`FrameSource` / `Encoder`:**

- Purpose: pluggable I/O boundary from `reco-core` to codecs.
- Examples: `crates/reco-core/src/source.rs` (`FrameSource`, `StereoFrame`,
  `YuvData`), `crates/reco-core/src/encoder.rs` (`Encoder::submit`).
- Pattern: backend code stays trait-free; thin adapters live in
  `crates/reco-io/src/adapters.rs:1`.

**`ControlIntent` / `PoseControl`:**

- Purpose: de-duplicate input handling across CLI/GUI/OBS.
- Examples: `crates/reco-control/src/lib.rs:61`, `pose_control.rs`,
  `intent_translator.rs`.
- Pattern: transports translate native events into `ControlIntent`; consumers
  dispatch to `PoseControl` + local handlers.

## Entry Points

**`reco` CLI binary:**

- Location: `crates/reco-cli/src/main.rs` (`fn main` at line 718; `[[bin]] name = "reco"`).
- Triggers: shell invocation.
- Responsibilities: installs tracing + panic hook + Ctrl-C handler, then dispatches
  subcommands `Stitch`, `Preview`, `Camera` (gstreamer), `Libcamera` (libcamera),
  `Calibrate`, `Info`, `Gopro` (gopro). Handlers live in `stitch.rs`, `preview.rs`,
  `camera.rs`, `libcamera_cmd.rs`, `calibrate.rs`.

**`reco-gui` Slint binary:**

- Location: `crates/reco-gui/src/main.rs`; UI markup `crates/reco-gui/ui/main.slint`.
- Triggers: desktop launch.
- Responsibilities: selects wgpu 28 backend via `BackendSelector::require_wgpu_28()`,
  captures Slint's device/queue through `set_rendering_notifier`, builds
  `PreviewBridge` for zero-copy rendering, manages playback/export/calibration.

**OBS plugin:**

- Location: `crates/reco-obs/src/lib.rs` (`obs_module_load` line 207 registers the
  source via `obs_register_source_s`); callbacks in `crates/reco-obs/src/source.rs`.
- Triggers: OBS loads the cdylib and instantiates the "Reco Panorama Stitcher" source.
- Responsibilities: `source_create`/`source_update`/`source_video_tick`/`source_video_render`,
  mouse pan/zoom, replay recording. Uses the push-first `StitchCore` with BGRA input
  (`crates/reco-obs/src/source.rs:24`).

**One-shot `StitchJob`:**

- Location: `crates/reco-io/src/stitch_job.rs:35` ("Layer 3 API").
- Triggers: `StitchJob::new(...).run(&interrupted)` from CLI or library consumers.
- Responsibilities: GPU init, zero-copy detection, encoder creation, decode-thread
  lifecycle, audio passthrough; `on_session` hooks wire autocam/telemetry.

**`StitchRenderer` (surface preview):**

- Location: `crates/reco-core/src/render/stitch_renderer.rs`.
- Triggers: `reco preview` (`crates/reco-cli/src/preview.rs`), reco-gui `PreviewBridge`.
- Responsibilities: wraps `StitchPipeline` with coverage + surface format handling for
  interactive per-tick rendering.

## Architectural Constraints

- **`reco-core` has no I/O dependencies.** No FFmpeg, no filesystem in its dependency
  graph; all I/O enters through the `FrameSource`/`Encoder` traits. Do not add a
  codec dependency to `crates/reco-core/Cargo.toml`.
- **`reco-core` stays domain-generic.** Ball/player/referee logic belongs in
  `reco-autocam`; `reco-core/src/detect/tracker.rs:27` states this explicitly.
- **Threading:** the render/encode loop is single-threaded per session; encoding runs
  on a spawned `AsyncEncodeThread` (`crates/reco-core/src/async_encode.rs`); decode
  runs on per-source OS threads; calibration fans out via `rayon`
  (`reco-calibrate` depends on `rayon`). GPU work is submitted to wgpu's queue and
  polled with `pollster`.
- **Global state:** avoid it. The only module-level mutable state is OBS plugin
  atomics (`crates/reco-obs/src/lib.rs:51,107`). Everything else is instance-owned.
- **Platform gating:** OS-specific modules are `#[cfg(target_os = ...)]`; e.g.
  `nvbuf_transform` is Linux-only (`crates/reco-core/src/lib.rs:87`),
  `SharedTextureSet` export is Linux-only (`crates/reco-core/src/session/mod.rs:45`).
- **Unsafe policy:** `reco-autocam` and `reco-control` are `#![forbid(unsafe_code)]`
  (`crates/reco-autocam/src/lib.rs:45`, `crates/reco-control/src/lib.rs:30`);
  `reco-calibrate` is `#![deny(unsafe_code)]` with narrow SIMD exceptions
  (`crates/reco-calibrate/src/lib.rs:12`); FFI/unsafe is confined to
  `reco-core` interop, `reco-detect`, and `reco-obs`.
- **Feature unification hazard:** consumers import `reco-detect` and `reco-autocam`
  with `default-features = false` so Jetson builds can opt out of the glibc-sensitive
  prebuilt ORT (`crates/reco-cli/Cargo.toml:49`, `crates/reco-gui/Cargo.toml:44`,
  `crates/reco-autocam/Cargo.toml:17`). Respect this pattern when adding consumers.
- **fuzz is workspace-excluded** because `libfuzzer-sys` needs nightly flags
  (`Cargo.toml` `exclude = ["fuzz"]`).

## Anti-Patterns

### Depending on `wgpu` directly in consumers

**What happens:** adding `wgpu` to a consumer's dependencies instead of using the
re-export.
**Why it's wrong:** version skew between the consumer's wgpu and `reco-core`'s
internal wgpu breaks texture/bind-group interop (the engine pins wgpu 28).
**Do this instead:** use `reco_core::wgpu` (re-exported at `crates/reco-core/src/lib.rs:74`)
or the `gpu::OutputFormat` enum for headless output.

### Reaching for the deleted `LiveStitchSession`

**What happens:** searching for or re-adding a live-only session type.
**Why it's wrong:** it was removed 2026-04-19; `StitchCore` is the canonical push API.
**Do this instead:** construct `reco_core::core::StitchCore` and call
`submit_frame_*_at_pose` (see note at `crates/reco-core/src/session/mod.rs:48`).

### Assuming `LShapeProjection` is the only geometry

**What happens:** hardcoding two-camera L-shape math in consumer code.
**Why it's wrong:** `Projection`/`CameraInput` are trait slots (`camera_count()`
is validated at build time) and a `CylindricalProjection` already exists.
**Do this instead:** branch on `Projection::name()` / `camera_count()` and drive
geometry through the pipeline (`crates/reco-core/src/projection/mod.rs:66`).

### Doing color conversion on the CPU

**What happens:** converting RGBA→NV12 or YUV→RGB with swscale on the host.
**Why it's wrong:** it was the main encode bottleneck; the GPU path cuts readback
bandwidth 2.7×.
**Do this instead:** render YUV/NV12 in and convert RGBA→NV12 with
`Nv12Converter` at the encode boundary (`crates/reco-core/src/gpu/nv12_converter.rs`).

## Error Handling

**Strategy:** typed `thiserror` enums per crate; recover where safe, propagate typed
errors to the application boundary.

**Patterns:**

- Public error enums are `Clone + Send + Sync` when they must cross threads:
  `PipelineError` (`crates/reco-core/src/render/pipeline.rs:38`),
  `GpuError` (`crates/reco-core/src/gpu/mod.rs:53`),
  `SessionError`/`StitchCoreError` (`crates/reco-core/src/core/types.rs`,
  `crates/reco-core/src/session/types.rs`). wgpu's non-`Clone` errors are flattened
  to `String` at the `From` boundary.
- Detection errors are logged at `warn!` and swallowed so a transient inference
  failure does not abort the render loop (`crates/reco-core/src/core/mod.rs:293`).
- OBS FFI callbacks are wrapped in `ffi_catch!` (`crates/reco-obs/src/lib.rs:85`),
  converting panics into safe default returns at the C ABI boundary.
- Input paths are validated *before* opening codecs:
  `reco_core::source::validate_input_path` (`crates/reco-core/src/source.rs:96`).

## Cross-Cutting Concerns

**Logging:** `tracing` workspace-wide. Binaries install `tracing-subscriber` with
`EnvFilter` (default `info,ort::logging=warn`) and bridge legacy `log::*` via
`tracing-log` (`crates/reco-cli/src/main.rs:43`). A `profiling` feature swaps in
`tracing-chrome` for `reco-trace.json` (`crates/reco-cli/src/main.rs:23`); zero-cost
when off via the `profile_scope!` macro (`crates/reco-core/src/lib.rs:49`).

**Validation:** dimensions clamped to `MAX_DIM` (8192) at CLI boundaries
(`crates/reco-cli/src/main.rs:824`); viewport/input validated in
`StitchPipeline::with_gpu` (`crates/reco-core/src/render/pipeline.rs:109`); replay
scale must be width%4==0, even height (`crates/reco-cli/src/main.rs:694`).

**Authentication:** not applicable — no server/auth surface. The only credential-like
concern is the optional GoPro HTTP control transport in
`crates/reco-control/src/gopro/` (feature `gopro`); it targets the camera's local
USB/WiFi endpoint, not a cloud account.

**Telemetry:** `TelemetryCollector` inside `StitchCore`/`StitchSession`
(`crates/reco-core/src/telemetry.rs`) records per-frame timing; autocam adds
`reco-calibrate/src/telemetry.rs` for IMU-driven sync. `PipelineEventSink` (JSONL)
captures detections/pose decisions (`crates/reco-io/src/jsonl_sink.rs`).

---

*Architecture analysis: 2026-10-01*
