# Reco Video Stitcher

## What This Is

An open-source, GPU-accelerated panoramic sports camera system. Reco stitches
video from multiple cameras into a single wide-angle panorama in real time, with
AI-driven camera control, for football clubs, amateur sports teams, and other
live-sports operators. This milestone adds a modern, native desktop GUI to
replace the current Slint-based binary.

## Core Value

Users can turn two camera feeds into a high-quality stitched panorama end-to-end
— import, calibrate, preview, export — without touching the command line, and
reliably even with mismatched consumer cameras.

## Requirements

### Validated

<!-- Existing capabilities, inferred from the codebase map (`.planning/codebase/`). -->

- ✓ GPU-accelerated real-time stitching engine (`reco-core`, wgpu 28) — existing
- ✓ Zero-copy GPU interop: CUDA/Vulkan, DMA-BUF, Metal, D3D11 (`reco-core/src/interop/`) — existing
- ✓ Camera-pixel ↔ panorama projection, lens model, viewport crop (`reco-core/src/projection/`, `render/`) — existing
- ✓ CLI: `stitch`, `preview`, `camera`, `libcamera`, `calibrate`, `info`, `gopro` (`reco-cli/src/main.rs`) — existing
- ✓ Multi-backend I/O: FFmpeg file/stream, GStreamer live cameras, libcamera (Pi CSI), V4L2 (`reco-io/`) — existing
- ✓ AI detection backends: ORT CPU/GPU, TensorRT, NCNN, CoreML/Metal (`reco-detect/`) — existing
- ✓ AI camerawork: trackers/panners/ROI filtering (`reco-autocam/`) — existing
- ✓ Stereo calibration: AKAZE features, matching/RANSAC, Nelder-Mead optimizer, telemetry sync (`reco-calibrate/`) — existing
- ✓ Transport-agnostic control vocabulary (`reco-control/`) — existing
- ✓ Zero-copy wgpu preview in the existing Slint GUI (`reco-gui/src/preview.rs`) — existing
- ✓ OBS Studio source plugin (`reco-obs/`) — existing

### Active

<!-- This milestone: the new GUI. -->

- [ ] A modern desktop GUI for Reco, built as a **Tauri 2** app (web frontend + existing Rust engine in-process)
- [ ] Import flow: select two video files and an associated camera match/calibration profile
- [ ] Guided calibration: wizard over two recorded clips using the `reco-calibrate` engine, with clear progress and actionable failure feedback
- [ ] In-app live camera capture as a calibration input path
- [ ] Load and save existing calibration profiles (`.json` match files)
- [ ] Real-time zero-copy preview of the stitched panorama from files **and** live cameras, with scrub/playback and left/right comparison
- [ ] Export final result with output presets (resolution/bitrate/codec), hardware encoding, stacked/side-by-side output, and trim range selection
- [ ] System info & diagnostics: GPU/backend info, camera info, logs/diagnostic bundle
- [ ] Diagnose and fix heterogeneous-camera feature-matching failure (top known blocker)
- [ ] End-to-end flow completes entirely in the GUI with no CLI required

### Out of Scope

- Replacing or rewriting the OBS Studio plugin — separate consumer, not part of this milestone
- Hosted/cloud processing service — local-first desktop chosen for performance and offline use
- Mobile app — the product targets mobile eventually, but not in this milestone
- Rewriting the GPU stitching engine (`reco-core`) — reuse in-process; do not fork
- Rewriting the CLI (`reco-cli`) — remains the scriptable interface

## Context

- **Brownfield:** a mature Cargo workspace (9 crates, edition 2024, Rust 1.92), AGPL-3.0, cross-platform (Windows/macOS/Linux). Full map in `.planning/codebase/`.
- **Existing GUI pain:** the current `reco-gui` Slint binary crashes, feels laggy, and has poor UX; friction is logged in `crates/reco-gui/FRICTION.md`.
- **Top user-reported blocker:** calibration feature matching fails on two heterogeneous phone clips (Xiaomi 11T Pro + Xiaomi 14T Pro). Root cause is algorithmic, not UI-only.
- **Test media available:** the problematic Xiaomi clips can be provided for testing and verification.
- **Engine constraints:** `reco-core` pins wgpu 28; consumers must use `reco_core::wgpu` and must not add a direct `wgpu` dependency (version skew breaks texture/bind-group interop).
- **Reusable seam:** `reco-control` provides a wgpu-free `ControlIntent` vocabulary, a natural seam for a new UI to drive the engine.

## Constraints

- **Tech stack:** Rust + wgpu 28 (pinned, drives MSRV 1.92) for the engine; Tauri 2 for the new GUI. Reuse `reco-core`/`reco-io`/`reco-calibrate`/`reco-detect`/`reco-autocam` in-process.
- **Performance:** real-time preview is the priority; avoid encoding/transferring preview frames. Keep the zero-copy GPU path.
- **Cross-platform:** Windows, macOS, Linux desktop at minimum; do not break existing Jetson/cloud/mobile build targets.
- **Compatibility:** must not break `reco-cli` or `reco-obs` consumers.
- **License:** AGPL-3.0-only; new dependencies must satisfy `deny.toml` gates.
- **Security/privacy:** process local media locally; no implicit uploads.

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Tauri 2 desktop app (web frontend + Rust engine in-process) | Modern UI without giving up native GPU performance; reuses the existing Rust engine | — Pending |
| Local-first, not hosted cloud | Preview performance, no upload time, offline use; users already run locally | — Pending |
| GUI v1 targets technical operators | Matches current user base; keeps onboarding/hand-holding scope contained | — Pending |
| Calibration reliability fix scoped as "needs research" | Feature-matching failure is algorithmic; root cause unknown until investigated | — Pending |
| Fate of existing `reco-gui` deferred | Decide replacement vs coexistence after the new GUI reaches parity | — Pending |

## Evolution

This document evolves at phase transitions and milestone boundaries.

**After each phase transition** (via `/gsd-transition`):
1. Requirements invalidated? → Move to Out of Scope with reason
2. Requirements validated? → Move to Validated with phase reference
3. New requirements emerged? → Add to Active
4. Decisions to log? → Add to Key Decisions
5. "What This Is" still accurate? → Update if drifted

**After each milestone** (via `/gsd-complete-milestone`):
1. Full review of all sections
2. Core Value check — still the right priority?
3. Audit Out of Scope — reasons still valid?
4. Update Context with current state

---
*Last updated: 2026-10-01 after initialization*
