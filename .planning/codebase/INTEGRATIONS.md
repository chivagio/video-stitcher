---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
# External Integrations

**Analysis Date:** 2026-10-01

## APIs & External Services

**Camera Control (OpenGoPro):**

- GoPro cameras via the OpenGoPro REST API - start/stop recording, sync settings, toggle webcam streaming, query status/telemetry. Feature-gated `gopro`.
  - SDK/Client: `reqwest` 0.12 blocking client (`rustls-tls`, `json`)
  - Auth: none - the camera is an unauthenticated local HTTP endpoint on the same USB/WiFi link
  - Implementation: `crates/reco-control/src/gopro/http.rs`, `crates/reco-control/src/gopro/mod.rs`
  - Addresses: USB `http://172.2{d1}.1{d2}{d3}.51:8080` (serial suffix), WiFi AP `http://10.5.5.9:8080`; RTSP webcam `rtsp://{host}:{port}/live`
  - CLI surface: `reco gopro --serial <suffix>` / `--start` / `--stop` / `--sports-preset` (`crates/reco-cli/src/main.rs`)

**Update Check (GitHub Releases):**

- GitHub REST API - checks `https://api.github.com/repos/reco-project/video-stitcher/releases/latest` for a newer version at GUI startup (background thread).
  - SDK/Client: `ureq` 3
  - Auth: none (anonymous, `User-Agent: reco-gui`)
  - Implementation: `crates/reco-gui/src/main.rs` (~line 1442); result opens the release page via the `open` crate.

**Opt-in Telemetry (self-hosted Cloud Run):**

- Anonymous usage events sent to `https://telemetry-ingestion-204135919265.us-central1.run.app/telemetry` (Google Cloud Run).
  - SDK/Client: `ureq` 3 (background thread, 5 s timeout), `uuid` v4 for the client id, `serde_json` payloads.
  - Auth: none. Fully opt-in — no events are sent unless the user enables telemetry in preferences. No PII, file paths, or video content.
  - When enabled, emits `app_open`, `context`, `source_info`, `bug_report`, `export_complete`, `export_error`, `calibration_complete`, `calibration_error`.
  - Implementation: `crates/reco-gui/src/telemetry_client.rs`; documented as "no telemetry by default" in `README.md` Privacy section.

**Developer/AI Tooling (external, not shipped):**

- GitHub Actions workflows call `api.github.com` (BtbN FFmpeg release resolution) and `api.nuget.org` (ONNX Runtime DirectML / DirectML packages) during CI/release builds.
  - Implementation: `.github/workflows/release.yml`, `.github/workflows/build-test.yml`, `.github/workflows/test-build-gui.yml`.

## Data Storage

**Databases:**

- None. No ORM, SQL, or embedded database dependency exists (`Cargo.lock` contains no `rusqlite`/`sqlx`/`diesel`/`postgres`/`redis`/`mongodb`/`rocksdb`).

**File Storage:**

- Local filesystem only. Video files, calibration JSON, JSONL pipeline-event logs, trajectory CSV, and stacked-video replay files are read/written via FFmpeg (`crates/reco-io/src/ffmpeg/`).
- Bundled read-only asset: `resources/profiles.cbor.gz` (Gyroflow lens profile DB, embedded at compile time by `crates/reco-calibrate/src/lens_database.rs`).
- User settings persisted as JSON under the platform config dir via the `directories` crate (`crates/reco-io/src/settings.rs`, feature `config`).
- Model/engine caches under `{platform_cache_dir}/reco/{trt-cache,coreml-cache}` (`crates/reco-detect/src/ort_session.rs`).

**Caching:**

- None external. In-process caches only: ONNX Runtime TensorRT/CoreML engine caches on disk (paths above), wgpu resource pools, and a VRAM lookahead pool in the stitch pipeline.

## Authentication & Identity

**Auth Provider:**

- None. The product has no accounts, login, or remote identity. The telemetry client uses a locally generated random UUID stored in settings as an anonymous `client_id` (`crates/reco-gui/src/telemetry_client.rs`).
- The OBS plugin is loaded inside OBS's process and relies on libobs being pre-loaded by the host (`crates/reco-obs/build.rs`, `crates/reco-obs/src/ffi.rs`).

## Monitoring & Observability

**Error Tracking:**

- No third-party error tracking (no Sentry, etc.). Errors surface as structured logs and, for the GUI, via the opt-in telemetry `bug_report` / `export_error` / `calibration_error` events.

**Logs:**

- `tracing` 0.1 with `tracing-subscriber` 0.3 (`env-filter`, `json`, `tracing-log`) in the CLI, GUI, and OBS plugin. `tracing-log` bridges legacy `log::*` call sites in `reco-core` / `reco-io` / `reco-calibrate`.
- Filter via `RUST_LOG`; FFmpeg native logging via `RECO_FFMPEG_LOG`; libva via `LIBVA_MESSAGING_LEVEL`.
- `profiling` feature adds `tracing-chrome` output (`reco-trace.json`, open in Perfetto) — zero-cost when off.
- OBS plugin diagnostics are routed into OBS's own log pane through the `blog` shim (`crates/reco-obs/src/blog_shim.c`, `obs_log.rs`).

## Media, Codec & Capture Integrations

**FFmpeg (`crates/reco-io/src/ffmpeg/`):**

- Decode/encode and demux/mux via `ffmpeg-next` 8 against FFmpeg 7.x (n7.1 ABI-compatible on Windows).
- Hardware **decode** backends (`DecodeBackend` in `crates/reco-io/src/ffmpeg/decoder.rs`): Software, CUDA (NVDEC), VA-API, VideoToolbox, D3D11VA. Disable with `RECO_NO_HWACCEL`.
- Hardware **encode** backends (`crates/reco-io/src/ffmpeg/encoder.rs`): `h264_nvenc`/`hevc_nvenc`/`av1_nvenc`, `h264_qsv`, `h264_videotoolbox`/`hevc_videotoolbox`, `h264_amf`/`hevc_amf`, `h264_vaapi`/`hevc_vaapi`/`av1_vaapi`, `h264_mf`, and software `libx264`/`libx265`/`libsvtav1`.
- Network output protocols (`crates/reco-io/src/output.rs`): `rtmp://` / `rtmps://` → FLV container, `srt://` → Matroska; RTSP ingest for streams.
- Audio extraction for calibration shells out to the `ffmpeg` CLI (`crates/reco-io/src/ffmpeg/calibration_io.rs`), with a path-prefix denylist (`http://`, `https://`, `concat:`, `pipe:`, `data:`) to block protocol abuse.
- Zero-copy interop: NVDEC/CUDA and D3D11 paths hand GPU frames directly into wgpu textures (`crates/reco-io/src/zero_copy.rs`, `crates/reco-core/src/interop/`).

**GStreamer (`crates/reco-io/src/gstreamer/`):**

- Live stereo ingest via `appsink`; platform pipelines in `camera.rs`: Jetson `nvarguscamerasrc` (NVMM/NV12), macOS `avfvideosrc`, Windows `mfvideosrc`, Linux V4L2 `/dev/videoN`.
- Device strings are validated to prevent pipeline injection (`validate_device_string`).
- Deepstream `nvmm` (NVMM memory) helper in `crates/reco-io/src/gstreamer/nvmm.rs`.

**V4L2 (`crates/reco-io/src/v4l2.rs`):**

- Raw MMAP capture of 10-bit RGGB Bayer from `/dev/videoN`, bypassing GStreamer and the NVIDIA ISP; targets the patched IMX477 driver (`VIDIOC_S_FMT`/`REQBUFS`/`QBUF`/`DQBUF` via `libc` ioctls). Feature `v4l2`.

**libcamera (`crates/reco-io/src/libcamera.rs`):**

- Spawns `rpicam-vid` subprocesses per camera (`--codec yuv420 -o -`) and reads raw YUV420P from stdout. Lowest-latency RPi CSI path. Feature `libcamera`.

**OBS Studio (`crates/reco-obs/`):**

- OBS source plugin (nativelib `cdylib`) against libobs 30.0.2 headers, ABI-compatible with OBS 32.x.
- bindgen-generated FFI from `obs.h` (`crates/reco-obs/build.rs`); frontend API hook (`obs-frontend-api.h`, cfg `have_frontend_api`) mirrors OBS Record/Stream state into the replay recorder.

## AI Inference Integrations

**ONNX Runtime (`ort` 2.0.0-rc.12):**

- Bundled runtime: ONNX Runtime **1.24.4** (CPU on Linux/macOS, DirectML on Windows) attached to GitHub releases; resolved via `ORT_DYLIB_PATH` or `libonnxruntime.{so,dylib,dll}` next to the executable (`crates/reco-detect/src/ort_session.rs`, issue #446 rpath/bundling).
- Execution providers (`crates/reco-detect/src/ort_session.rs`, probed in `probe.rs`): TensorRT → CUDA → CoreML → DirectML → CPU, with graceful fallback.
- `load-dynamic` probes the dylib out-of-band before entering ort to avoid its self-deadlock on a missing runtime.

**Native TensorRT (`tensorrt-native`):**

- C++ wrapper `crates/reco-detect/csrc/tensorrt_wrapper.cpp` + Rust `crates/reco-detect/src/detectors/trt/`, linking `nvinfer` + `cudart`. Targets Jetson `.engine` files with no ORT glibc dependency. Build paths overridable via `TENSORRT_LIB_DIR`, `CUDA_HOME`, `CUDA_LIB_DIR`.

**NVIDIA CUDA / NPP (`crates/reco-detect/src/cuda_kernels.rs`, `npp_interop.rs`):**

- CUDA kernels for normalize + HWC→CHW and P010→NV12; NPP libraries are `dlopen`-ed at runtime via `libloading` so binaries still start without CUDA installed (report `NotAvailable`).

**NCNN (`ncnn` feature, `crates/reco-detect/src/detectors/ncnn.rs`):**

- Tencent NCNN static library for ARM/RPi5 and mobile; build from source, locate via `NCNN_DIR`; links `stdc++` and `gomp`.

**Apple CoreML / Metal (`coreml` feature):**

- CoreML model wrapping (`crates/reco-detect/src/coreml_inference.rs`) and Metal compute YOLO preprocess (`crates/reco-detect/src/metal_compute.rs`) via `objc2-core-ml`, `objc2-metal`, `metal` 0.33. CoreML engine cache under `{cache}/reco/coreml-cache`.

## Calibration Integrations

**telemetry-parser (git dependency):**

- IMU/GPMF telemetry extraction from camera files for temporal sync and lens lookup.
  - Source: `https://github.com/AdrianEddy/telemetry-parser.git` pinned to rev `2f4218b` (`crates/reco-calibrate/Cargo.toml`); allowlisted in `deny.toml` along with its transitive git deps.
  - Implementation: `crates/reco-calibrate/src/telemetry.rs`.

**Gyroflow lens profiles:**

- Bundled database converted from `github.com/gyroflow/lens_profiles` (CC0-1.0), shipped as `resources/profiles.cbor.gz` and loaded by `crates/reco-calibrate/src/lens_database.rs`.
- Converter tool: `scripts/convert-gyroflow-profiles.py`.

**Audio sync:**

- Audio cross-correlation for temporal sync uses `realfft` on PCM extracted via the `ffmpeg` CLI (`crates/reco-io/src/ffmpeg/calibration_io.rs`).

## Environment Configuration

**Required env vars (build or runtime):**

- Build: `FFMPEG_DIR` (Windows), `OBS_INCLUDE_DIR` (when libobs is not at `/usr/include/obs`), `CUDA_HOME` / `CUDA_LIB_DIR` / `TENSORRT_LIB_DIR` (native TensorRT), `NCNN_DIR` (NCNN).
- Runtime: `RUST_LOG`, `RECO_FFMPEG_LOG`, `LIBVA_MESSAGING_LEVEL`, `RECO_NO_HWACCEL`, `WGPU_BACKEND`, `ORT_DYLIB_PATH`, `RECO_CONFIG_DIR`, `RECO_VRAM_BUDGET_GB`.
- CI/automation (GUI, `automation` feature): `RECO_AUTOLOAD`, `RECO_AUTOEXPORT`, `RECO_AUTOEXPORT_MODEL`, `RECO_AUTOEXPORT_LOOKAHEAD`, `RECO_AUTOEXPORT_REPEAT`.

**Secrets location:**

- No secrets or credential files are committed. There are no `.env` files in the repository and no application secrets are required at runtime (the GitHub/GitHub-Actions token in CI is the standard `secrets.GITHUB_TOKEN`). `.gitignore` excludes `*.onnx`, `*.engine`, `*.pt`, `*.mp4`, `.cargo/`, and `.claude/`.

## Webhooks & Callbacks

**Incoming:**

- None (no server listens for external webhooks). `reco-gui` writes `resources/roi_editor.html` to a temp directory and opens it in the local browser via `open::that` for ROI polygon editing; there is no network callback endpoint.
- GitHub Actions trigger events are defined in `.github/workflows/` (push, pull_request, tags `v*`, `workflow_dispatch`, and CLA `issue_comment`/`pull_request_target`).

**Outgoing:**

- RTMP/RTMPS, SRT, and RTSP streams for live output (`crates/reco-io/src/output.rs`).
- GoPro OpenGoPro HTTP/RTSP commands to local cameras (`crates/reco-control/src/gopro/`).
- GitHub releases API (update check), self-hosted Cloud Run telemetry endpoint (opt-in), and CI calls to GitHub/nuget APIs (build-time only).

---

*Integration audit: 2026-10-01*
