# Roadmap: Reco Video Stitcher

## Overview

This milestone adds a modern Tauri 2 desktop GUI over the existing Rust/wgpu stitching
engine, so an operator can take two (often mismatched) camera feeds through
**import → calibrate → preview → export** without touching the command line. It is a
brownfield integration, not a greenfield product: the engine stays in-process and
unrewritten, and almost all of the risk concentrates at the seam between the engine and
the new shell. Two genuinely unknown items are deliberately isolated — native
wgpu-surface compositing with the Tauri webview (especially Linux/Wayland) and
heterogeneous-camera calibration reliability. Everything else is standard application
work once the engine-session + message-passing spine exists.

## Phases

**Phase Numbering:**

- Integer phases (1, 2, 3): Planned milestone work
- Decimal phases (2.1, 2.2): Urgent insertions (marked with INSERTED)

Decimal phases appear between their surrounding integers in numeric order.

- [x] **Phase 1: Walking Skeleton & Native-Surface Gate** - Tauri shell + engine session + proven native-surface compositing, with a thin import→preview→export path and packaging credentials kicked off (completed 2026-10-01)
- [ ] **Phase 2: Zero-Copy Preview, Playback & Pose** - Real-time panorama preview from files with transport, source/panorama toggle, pose controls, and a runtime-swappable presenter
- [ ] **Phase 3: Import & Guided Calibration Wizard** - Two-file import with metadata provenance and warnings; guided calibration with progress, cancel, and a result scorecard; profile load/save
- [ ] **Phase 4: Calibration Reliability & Failure Diagnostics** - Mismatched-camera algorithm fixes plus plain-language failure causes, readiness checks, sync diagnostics, and a debug inspector
- [ ] **Phase 5: Export, Projects, Diagnostics & Distribution** - Export presets/encoders/trim/variants, `.reco` projects, system info/logs/diagnostics bundle, and signed per-platform installers

## Phase Details

### Phase 1: Walking Skeleton & Native-Surface Gate

**Goal**: A Tauri 2 app that links the engine crates in-process, proves that a native `wgpu::Surface` composited with the Tauri webview renders the stitched panorama on Windows, macOS, and Linux X11 (go/no-go), establishes the single-owner engine-worker boundary, and carries two hardcoded clips through the thinnest import→preview→export path — with packaging/signing credential acquisition started for its long lead time.
**Mode:** mvp
**Depends on**: Nothing (first phase)
**Requirements**: FOUND-01, FOUND-02, FOUND-03, FOUND-04, FOUND-05, FOUND-06
**Success Criteria** (what must be TRUE):

  1. User can launch the app on Windows, macOS, and Linux X11 and see a stitched panorama rendered by a native wgpu surface composited with the Tauri webview
  2. Two test clips complete import → preview → export in one session, proving the engine crates are linked in-process and all UI↔engine communication is message passing
  3. The panorama recovers after a simulated device loss with no black preview and no hang
  4. Closing the app tears down the engine session cleanly — no process hang and no unbounded VRAM growth
  5. CI fails if any GUI crate adds a direct `wgpu` dependency or the workspace wgpu pin drifts from 28

**Plans:** 5/5 plans complete
Plans:
**Wave 1**

- [x] 01-01-PLAN.md — Tracer: Tauri host crate + native child-view surface renders one stitched frame (falsifies A1/A3); platform dispatch; adapter-retention seam

**Wave 2** *(blocked on Wave 1 completion)*

- [x] 01-02-PLAN.md — Single-owner engine worker + typed message-passing command/event protocol
- [x] 01-03-PLAN.md — CI wgpu-pin / no-direct-wgpu gate + cargo-deny + workspace- job wiring

**Wave 3** *(blocked on Wave 2 completion)*

- [x] 01-04-PLAN.md — Minimal UI (3 buttons + event log) and the thin import→preview→export path

**Wave 4** *(blocked on Wave 3 completion)*

- [x] 01-05-PLAN.md — DeviceLost recovery, clean teardown, four-platform gate report, packaging credentials

### Phase 2: Zero-Copy Preview, Playback & Pose

**Goal**: The production preview experience — a real-time, zero-copy stitched panorama from the selected files, with playback transport, source/panorama comparison, and renderer-owned pose controls, delivered through a presenter that swaps at runtime (native compositing → separate preview window → throttled readback) with a defined Linux/Wayland posture.
**Mode:** mvp
**Depends on**: Phase 1
**Requirements**: PREV-01, PREV-02, PREV-03, PREV-04, PREV-05
**Success Criteria** (what must be TRUE):

  1. User sees a real-time, zero-copy preview of the stitched panorama from the selected files, with no frame pixels crossing IPC
  2. User can play, pause, scrub, frame-step, and loop the preview
  3. User can toggle between the source view and the stitched panorama
  4. User can pan, zoom, and adjust FOV on the panorama
  5. When native compositing is unavailable, the preview presenter falls back automatically to a separate preview window, then to a throttled readback, with a defined Wayland posture

**Plans:** 5 plans
Plans:
**Wave 1**

- [ ] 02-01-PLAN.md — Tracer: full-window transparent chrome over the native child view (z-order + pointer events) + ChromeState/ViewportRect::for_chrome + headless probe script

**Wave 2** *(blocked on Wave 1 completion)*

- [ ] 02-02-PLAN.md — Paced transport session loop, play/pause/scrub/step/loop, pose tick + FOV plumbing, chrome/viewport reconfigure

**Wave 3** *(blocked on Wave 2 completion)*

- [ ] 02-03-PLAN.md — Widened presenter seam, SeparateWindowPresenter + ReadbackPresenter, capability probe/fallback chain/override + WARN, presenter posture doc

**Wave 4** *(blocked on Wave 3 completion)*

- [ ] 02-04-PLAN.md — Source↔panorama toggle: additive reco-core tile renderer + FRICTION entry, ViewMode protocol, boundary/precision tests
- [ ] 02-05-PLAN.md — Svelte 5 + Tailwind preview shell (transport/pose/view/presenter/log) + CI frontend-build wiring

**UI hint**: yes

### Phase 3: Import & Guided Calibration Wizard

**Goal**: The core operator flow — pick two files, understand their inputs and risks, run a guided calibration wizard over the existing `reco-calibrate` engine with honest progress and a trustworthy result scorecard, and load/save `.json` calibration profiles.
**Mode:** mvp
**Depends on**: Phase 1
**Requirements**: IMPT-01, IMPT-02, IMPT-03, IMPT-04, IMPT-05, IMPT-06, CALB-01, CALB-02, CALB-03
**Success Criteria** (what must be TRUE):

  1. User can select two video files via native file pickers or drag-and-drop, and sees each input's resolution, fps, duration, and codec with any estimated value marked as estimated
  2. User is warned before a likely-doomed run when inputs are incompatible, and can detect or override the lens profile for each input
  3. User can load an existing `.json` calibration profile and save the current one
  4. User can run the guided calibration wizard over two clips with per-stage progress and an intra-step heartbeat (it never looks stuck), and can cancel a running calibration
  5. User sees a calibration result scorecard covering confidence, residual error, match count, frames used, resolved lens profile and source, and sync method and confidence

**Plans**: TBD
**UI hint**: yes

### Phase 4: Calibration Reliability & Failure Diagnostics

**Goal**: Make calibration trustworthy on mismatched consumer cameras — algorithmic reliability work in the pipeline plus the diagnostic surface that explains failures, checks heterogeneous readiness, reports sync provenance, and exposes a debug inspector for inspecting and framing a bad result. Verified against the real Xiaomi clips.
**Mode:** mvp
**Depends on**: Phase 3
**Requirements**: CALB-04, CALB-05, CALB-06, CALB-07, CALB-08, CALB-09
**Success Criteria** (what must be TRUE):

  1. On failure, the user sees a plain-language cause and a suggested fix derived from the typed engine error and stage metrics — never a silent fallback and never a bare error code
  2. The system performs a heterogeneous-camera readiness check and reports specific, actionable reasons before a doomed run
  3. The system reports temporal-sync diagnostics with provenance (IMU → audio → manual) and a confidence value
  4. Calibration succeeds on the mismatched Xiaomi test clips, using exposure normalization before feature extraction, adaptive match-ratio with geometric verification, multi-scale matching, and rolling-shutter-aware filtering
  5. User can inspect a failed calibration through a feature-match overlay and residual map, and can visualize and drag-edit the field ROI polygon

**Plans**: TBD
**UI hint**: yes

### Phase 5: Export, Projects, Diagnostics & Distribution

**Goal**: Ship the app — export the finished result with presets, hardware encoding, trim, and stacked/side-by-side variants; persist work as a non-destructive `.reco` project; expose system info, structured logs, and a local diagnostics bundle; and distribute signed/notarized installers with their runtime prerequisites.
**Mode:** mvp
**Depends on**: Phase 2, Phase 3
**Requirements**: EXPT-01, EXPT-02, EXPT-03, EXPT-04, EXPT-05, EXPT-06, PROJ-01, DIAG-01, DIAG-02, DIAG-03, DIAG-04, DIAG-05
**Success Criteria** (what must be TRUE):

  1. User can choose an export preset (resolution, codec, quality, bitrate), select a hardware encoder with explicit visible software fallback, set a trim range, and export side-by-side/stacked variants with predictable naming that handles collisions
  2. User sees export progress with an ETA and can cancel a running export
  3. User can save work as a `.reco` project and reopen it with inputs, lens overrides, calibration, pose, and export settings intact
  4. User can view system info (GPU/backend/driver, available encoders, camera/device info), view structured logs in-app, and export a one-click, redacted, local-only diagnostics bundle
  5. The app installs from signed/notarized per-platform packages (macOS Developer ID + notarization, Windows Authenticode + WebView2 bootstrap, Linux packages) with runtime prerequisites documented and shipped

**Plans**: TBD
**UI hint**: yes

## Progress

**Execution Order:**
Phases execute in numeric order: 1 → 2 → 3 → 4 → 5

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 1. Walking Skeleton & Native-Surface Gate | 5/5 | Complete    | 2026-10-01 |
| 2. Zero-Copy Preview, Playback & Pose | 5/5 planned | Ready to execute | - |
| 3. Import & Guided Calibration Wizard | TBD | Not started | - |
| 4. Calibration Reliability & Failure Diagnostics | TBD | Not started | - |
| 5. Export, Projects, Diagnostics & Distribution | TBD | Not started | - |
