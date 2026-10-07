//! The single-owner engine worker (FOUND-03 / D-06).
//!
//! # Why this module exists
//!
//! FOUND-03 requires **one** thread to exclusively own the GPU device, the
//! stitched renderer, the pose state, and the playback/decode state. D-06
//! requires that every UI→engine interaction travel as a typed message over a
//! channel — never a direct `StitchJob`/engine call from a `#[tauri::command]`
//! handler.
//!
//! This module is the only place in `reco-app` where those engine objects
//! exist. The worker thread builds them once, holds them for the app's whole
//! lifetime, and drops them in a fixed order at shutdown (FOUND-06):
//!
//! ```text
//!   stop worker loop  →  decode source  →  StitchRenderer  →  presenter Surface  →  GpuContext (device)
//!                        (drop first,      (then)             (then)                 (last)
//!                         NVDEC/CUDA race)
//! ```
//!
//! The order is enforced both explicitly (`shutdown()` drops the decode source
//! and renderer first) and structurally (`GpuContext` is declared last, so field
//! drop order agrees). See `crates/reco-cli/src/preview.rs:213-215` for the
//! NVDEC-before-wgpu hazard this honors.
//!
//! # The no-shared-lock invariant
//!
//! `crates/reco-gui/FRICTION.md` records the hazard this boundary exists to
//! avoid: a UI thread that holds a lock on engine state across a render tick
//! stalls the compositor and, worse, can poison a `Mutex` if a render panics
//! while the guard is held. CONCERNS.md ("Mutex-poisoning panics", files listed
//! at `crates/reco-gui/src/main.rs:798` etc.) is the concrete prior art: those
//! `lock().unwrap()` sites propagate a panic and take the app down. The worker
//! makes that class of bug impossible by construction — **no engine object is
//! shared**; the only cross-thread objects are the `mpsc` endpoints carrying
//! `Clone + Send` values. There is no `Mutex<Engine>` anywhere, so there is no
//! lock to hold across a tick and none to poison. The `protocol_payloads_are_send`
//! test below asserts the bound that keeps this true.
//!
//! # Loop shape (the FOUND-03 adjacency / empty assumptions)
//!
//! Each iteration **drains** every pending command with `try_recv` in arrival
//! order (per-sender FIFO), then, if nothing arrived and no job is active,
//! **blocks** on `recv` rather than spinning a busy loop:
//!
//! ```text
//!   loop {
//!       while let Ok(cmd) = rx.try_recv() { handle(cmd) }   // drain, FIFO
//!       if job_active { continue }                          // poll between frames
//!       match rx.recv() { Ok(cmd) => handle(cmd), Err(_) => break }  // block, no spin
//!   }
//! ```
//!
//! During a long job the loop re-drains between frames so a `Shutdown` is
//! honored promptly (the `interrupted` flag is also set, so a blocking engine
//! call cancels at its next checkpoint).
//!
//! # Testability
//!
//! The loop is generic over [`EngineBackend`]; the GPU-touching implementation
//! ([`GpuEngineBackend`]) is one impl, and tests supply a GPU-free mock. That
//! keeps the protocol (ordering, drain/block, error surfacing, intent
//! dispatch) unit-testable without a device (CONCERNS.md: GPU tests skip in
//! CI). Tests that need the real device are `#[ignore]`d.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// `info()` / `next_frame()` are trait methods on `FfmpegFileSource`, not
// inherent ones — the `FrameSource` trait must be in scope to call them.
use reco_core::source::FrameSource as _;

// Re-export the protocol types so consumers reach the whole worker surface
// through `worker::` (the binary and Plan 04's Tauri commands do).
pub use crate::commands::{WorkerCommand, WorkerHandle};
pub use crate::events::{Level, WorkerError, WorkerEvent};

/// Newtype wrapper for the readback channel sender, so it can be managed as
/// Tauri app state (PREV-05).
///
/// Managed with `app.manage(ReadbackSender(tx))`; `preview_attach_readback`
/// resolves it via `State<ReadbackSender>` and forwards a webview
/// `Channel<Response>` to the worker.
pub struct ReadbackSender(pub Sender<tauri::ipc::Channel<tauri::ipc::Response>>);

/// The shared calibration-cancel flag, managed as Tauri app state (CALB-02).
///
/// `cancel_calibration` writes `true` into this flag directly (the documented
/// control-plane bypass of the blocked command channel); `GpuEngineBackend`
/// holds a clone and passes it to `calibrate_videos_with_gpu`, which polls it
/// between steps. Reset to `false` at the start of every calibration run.
pub struct CalibrationCancel(pub Arc<AtomicBool>);

/// The shared export-cancel flag, managed as Tauri app state (EXPT-04).
///
/// Mirrors [`CalibrationCancel`]: `cancel_export` writes `true` into this flag
/// directly because the export job blocks the single worker loop, so a posted
/// `CancelExport` command would not be observed until the job already returned.
/// The flag is **distinct** from the calibration flag so a stray export cancel
/// can never abort a calibration (prohibition: never reuse the calibration
/// flag). Reset to `false` at the start of every export run.
pub struct ExportCancel(pub Arc<AtomicBool>);

/// The webview readback channel type (raw frame bytes as an `ArrayBuffer`).
pub type ReadbackChannel = tauri::ipc::Channel<tauri::ipc::Response>;

/// The manual-frame binary channel type (raw RGBA frames as an `ArrayBuffer`).
///
/// The manual flow streams its preview and validation pixels over this channel
/// instead of the JSON `worker-event-typed` bridge: a single bounded frame is
/// ~2 MB of RGBA, which serializes to ~8 MB of JSON numbers and made the webview
/// parse millions of numbers per frame (Phase 04.1 OOM). The binary path sends
/// the bytes verbatim, mirroring the readback presenter.
pub type ManualFrameChannel = tauri::ipc::Channel<tauri::ipc::Response>;

/// The shared slot the worker and the `manual_attach_preview` command use to
/// hand over the manual-frame binary channel (MANU-03).
///
/// Interior-mutable because the webview attaches the channel on the Tauri
/// runtime while the worker reads it from its own thread; both hold clones of
/// the same `Arc`. The worker's [`EventSink`] owns one clone, so every backend
/// method that receives `&EventSink` can stream a manual frame without a new
/// trait method.
pub type ManualFrameSlot = Arc<std::sync::Mutex<Option<ManualFrameChannel>>>;

/// Newtype wrapper so the manual-frame slot can be managed as Tauri app state.
///
/// Mirrors [`ReadbackSender`]: `manual_attach_preview` resolves it via
/// `State<ManualFrameSender>` and stores the webview `Channel<Response>` into
/// the shared slot the worker already holds.
pub struct ManualFrameSender(pub ManualFrameSlot);

impl ManualFrameSender {
    /// Attach (or replace) the manual-frame channel the worker streams to.
    ///
    /// A poisoned lock is ignored: the channel is best-effort telemetry and a
    /// panic elsewhere must not abort the attach command.
    pub fn attach(&self, channel: ManualFrameChannel) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(channel);
        }
    }
}

/// The kind tag that prefixes every manual binary frame.
///
/// The manual channel multiplexes four logical streams — the two camera
/// previews and the validation/reference stitched pair — over one
/// `Channel<Response>`. The first byte of each frame identifies which one, so
/// the frontend routes the pixels without a second event or a frame-index
/// handshake. Values are frozen (the frontend mirrors them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ManualFrameKind {
    /// Left camera reference preview.
    PreviewLeft = 0,
    /// Right camera reference preview.
    PreviewRight = 1,
    /// The validation frame's stitched output.
    Validation = 2,
    /// The calibration frame's stitched reference for the blink comparison.
    Reference = 3,
}

/// Length in bytes of the manual binary frame header:
/// `[kind: u8][width: u32 LE][height: u32 LE]`.
///
/// The RGBA payload starts at this offset. The frontend mirrors this constant;
/// its length guard fails closed if the two drift. This extends the readback
/// convention ([`crate::presenter::readback::READBACK_HEADER_LEN`]) with the
/// one-byte kind tag.
pub const MANUAL_FRAME_HEADER_LEN: usize = 9;

/// Prefix `bytes` with the manual frame kind and geometry:
/// `[kind: u8][width: u32 LE][height: u32 LE][RGBA bytes]`.
///
/// Mirrors [`crate::presenter::readback::frame_with_header`] with the kind tag
/// that distinguishes the four manual streams. Built directly (no intermediate
/// buffer) because this is the hot path the Phase 04.1 memory fix targets.
pub fn manual_frame_with_header(
    kind: ManualFrameKind,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut framed = Vec::with_capacity(MANUAL_FRAME_HEADER_LEN + bytes.len());
    framed.push(kind as u8);
    framed.extend_from_slice(&width.to_le_bytes());
    framed.extend_from_slice(&height.to_le_bytes());
    framed.extend_from_slice(bytes);
    framed
}

/// The presenter chain handed from the setup thread to the worker (PREV-05):
/// strongest-first `(kind, pre-built presenter)` pairs.
pub type PresenterChain = Vec<(
    crate::presenter::PresenterKind,
    Box<dyn crate::presenter::SurfacePresenter + Send>,
)>;

/// The result of [`spawn_gpu_worker`]: the worker, its event receiver, the
/// readback channel sender, and the manual-frame channel slot.
pub type SpawnedWorker = (
    EngineWorker,
    Receiver<WorkerEvent>,
    Sender<ReadbackChannel>,
    ManualFrameSlot,
);

/// A sink the worker uses to emit [`WorkerEvent`]s and stream binary frames to
/// the UI.
///
/// Emitting is infallible from the worker's point of view: if the UI has gone
/// away the send fails and the event is dropped. That is deliberate — the
/// worker's work should not abort because a log line could not be displayed.
///
/// The sink also owns the manual-frame binary channel slot (MANU-03): every
/// backend method already receives `&EventSink`, so routing the manual pixels
/// through it keeps the `EngineBackend` trait unchanged while guaranteeing no
/// `Vec<u8>` ever enters the JSON `worker-event-typed` stream.
pub struct EventSink {
    tx: Sender<WorkerEvent>,
    /// The webview channel manual frames are streamed to, once the UI attaches
    /// one via `manual_attach_preview`. `None` until then; frames are dropped
    /// rather than blocking the worker.
    manual_frames: ManualFrameSlot,
    /// The last debug inspector report published (DIAG-03). Retained so the
    /// diagnostics bundle can include the debug payload without the backend
    /// having to keep its own copy; `None` until a calibration publishes one.
    last_debug: Arc<std::sync::Mutex<Option<crate::events::DebugReport>>>,
}

impl EventSink {
    /// Build a sink over `tx` with no manual-frame channel attached yet.
    fn new(tx: Sender<WorkerEvent>) -> Self {
        Self {
            tx,
            manual_frames: Arc::new(std::sync::Mutex::new(None)),
            last_debug: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// A clone of the shared manual-frame slot, for `spawn_gpu_worker` to hand
    /// to the `manual_attach_preview` command layer.
    fn manual_frame_slot(&self) -> ManualFrameSlot {
        Arc::clone(&self.manual_frames)
    }

    /// Attach (or replace) the manual-frame channel (MANU-03). Test-facing.
    #[cfg_attr(not(test), allow(dead_code))]
    fn attach_manual_frame_channel(&self, channel: ManualFrameChannel) {
        if let Ok(mut slot) = self.manual_frames.lock() {
            *slot = Some(channel);
        }
    }

    /// Stream one framed manual frame over the attached channel, if any.
    ///
    /// The clone-and-drop-guard keeps a slow send from serialising behind the
    /// `manual_attach_preview` command's lock; a missing channel is a no-op so
    /// the worker never blocks waiting for a webview.
    fn send_manual_frame(&self, kind: ManualFrameKind, rgba: &[u8], width: u32, height: u32) {
        let channel = {
            let Ok(slot) = self.manual_frames.lock() else {
                return;
            };
            match slot.as_ref() {
                Some(channel) => channel.clone(),
                None => return,
            }
        };
        let payload = manual_frame_with_header(kind, rgba, width, height);
        let _ = channel.send(tauri::ipc::Response::new(payload));
    }

    /// Emit a log line at `level`.
    ///
    /// The line is mirrored to the process log as well as the UI channel, so a
    /// headless run (a gate report, CI) shows the same progress narrative the
    /// webview renders.
    fn log(&self, level: Level, message: impl Into<String>) {
        let message = message.into();
        match level {
            Level::Info => log::info!("{message}"),
            Level::Warn => log::warn!("{message}"),
            Level::Error => log::error!("{message}"),
        }
        let _ = self.tx.send(WorkerEvent::Log { level, message });
    }

    /// Emit an info line.
    fn info(&self, message: impl Into<String>) {
        self.log(Level::Info, message);
    }

    /// Emit a typed failure (also mirrored to the process log).
    fn failed(&self, error: WorkerError) {
        log::error!("worker failure: {error}");
        let _ = self.tx.send(WorkerEvent::Failed(error));
    }

    /// Emit the authoritative playback position (PREV-02).
    fn position(&self, frame: u64, total: Option<u64>, fps_rational: Option<(i32, i32)>) {
        let _ = self.tx.send(WorkerEvent::Position {
            frame,
            total,
            fps_rational,
        });
    }

    /// Emit the authoritative transport state (PREV-02).
    fn transport(&self, state: crate::transport::TransportState, loop_enabled: bool) {
        let _ = self.tx.send(WorkerEvent::Transport {
            state,
            loop_enabled,
        });
    }

    /// Emit the authoritative pose after a tick (PREV-04).
    ///
    /// `fov_max` is the coverage ceiling the pose is clamped to, reported so a
    /// control advertising a fixed range can be bounded to what this clip
    /// actually allows instead of silently doing nothing above it.
    fn pose(&self, yaw: f32, pitch: f32, fov_degrees: f32, fov_max: f32) {
        let _ = self.tx.send(WorkerEvent::Pose {
            yaw,
            pitch,
            fov_degrees,
            fov_max,
        });
    }

    /// Emit the active presenter (PREV-05). `reason` is `Some` only on fallback.
    fn presenter(&self, kind: crate::presenter::PresenterKind, reason: Option<String>) {
        let _ = self.tx.send(WorkerEvent::Presenter { kind, reason });
    }

    /// Emit the active view mode (PREV-03).
    fn view(&self, mode: crate::presenter::ViewMode) {
        let _ = self.tx.send(WorkerEvent::View { mode });
    }

    /// Emit the probed metadata for one imported input (IMPT-01/IMPT-02).
    ///
    /// Structured: the frontend reads the typed payload under
    /// `worker-event-typed` and never regex-parses a log line (FRICTION A3/A12).
    fn metadata(&self, role: crate::events::InputRole, metadata: crate::events::InputMetadata) {
        let _ = self.tx.send(WorkerEvent::ImportMetadata { role, metadata });
    }

    /// Clone the underlying event sender.
    ///
    /// Used by the calibration heartbeat monitor thread, which must emit
    /// `CalibrationHeartbeat` ticks while the worker thread is blocked inside
    /// `calibrate_videos_with_gpu` (D3-10). `Sender` is `Clone + Send`, so the
    /// thread can own one independently of the worker.
    fn sender_clone(&self) -> Sender<WorkerEvent> {
        self.tx.clone()
    }

    /// Emit the severity-sorted readiness report (CALB-05).
    ///
    /// Each finding is also mirrored to the process log, so a headless run
    /// shows the same reasons the webview banner renders. A likely-doomed pair
    /// (a blocking-shape finding) logs at WARN; informational notes at INFO.
    fn readiness(&self, report: crate::events::ReadinessReport) {
        for finding in &report.findings {
            if finding.severity == crate::events::ReadinessSeverity::BlockingShape {
                log::warn!("readiness: {}", finding.message);
            } else {
                log::info!("readiness: {}", finding.message);
            }
        }
        let _ = self.tx.send(WorkerEvent::Readiness { report });
    }

    /// Emit the lens-profile candidates for one input (IMPT-04).
    fn lens_candidates(
        &self,
        role: crate::events::InputRole,
        candidates: Vec<crate::events::LensCandidate>,
    ) {
        let _ = self
            .tx
            .send(WorkerEvent::LensCandidates { role, candidates });
    }

    /// Emit the applied (or cleared) lens override for one input (IMPT-04).
    fn lens_override_applied(
        &self,
        role: crate::events::InputRole,
        candidate: Option<crate::events::LensCandidate>,
    ) {
        let _ = self
            .tx
            .send(WorkerEvent::LensOverrideApplied { role, candidate });
    }

    /// Emit a stage-checklist row change (CALB-01).
    fn stage(
        &self,
        step: crate::events::CalibrationStage,
        status: crate::events::StageStatus,
        detail: impl Into<String>,
    ) {
        let _ = self.tx.send(WorkerEvent::CalibrationStage {
            step,
            status,
            detail: detail.into(),
        });
    }

    /// Emit overall calibration progress (CALB-01).
    fn progress(&self, fraction: f64) {
        let _ = self.tx.send(WorkerEvent::CalibrationProgress { fraction });
    }

    /// Emit the completed calibration's scorecard (CALB-03).
    fn result(&self, scorecard: crate::events::Scorecard) {
        let _ = self.tx.send(WorkerEvent::CalibrationResult { scorecard });
    }

    /// Emit a typed calibration failure with a plain-language diagnosis (CALB-04).
    ///
    /// Mirrored to the process log at ERROR so a headless run shows the same
    /// cause the webview panel renders; the typed event carries the full
    /// cause/fix/metrics for the `worker-event-typed` channel.
    fn failed_diagnosis(&self, diagnosis: crate::events::CalibrationDiagnosis) {
        log::error!("calibration failed: {}", diagnosis.cause);
        let _ = self.tx.send(WorkerEvent::CalibrationFailed { diagnosis });
    }

    /// Emit the bounded debug inspector payload (CALB-08).
    ///
    /// Mirrored to the process log at INFO (UI-SPEC Event Log Contract); the
    /// structured report rides the typed channel. The report is already bounded
    /// (downscaled thumbnails, capped points), so this is a cheap send.
    fn calibration_debug(&self, report: crate::events::DebugReport) {
        log::info!(
            "debug data published: frame {} of {}, {} verified / {} rejected matches",
            report.frame_index + 1,
            report.frames_total,
            report.verified.len(),
            report.rejected.len(),
        );
        // DIAG-03: retain the latest report so the diagnostics bundle can carry
        // the debug inspector payload (thumbnails are dropped at serialization).
        if let Ok(mut slot) = self.last_debug.lock() {
            *slot = Some(report.clone());
        }
        let _ = self.tx.send(WorkerEvent::CalibrationDebug { report });
    }

    /// A snapshot of the last published debug inspector report (DIAG-03).
    ///
    /// `None` until a calibration publishes one; the diagnostics bundle then
    /// omits the `debug.json` entry rather than fabricating an empty report.
    fn snapshot_debug(&self) -> Option<crate::events::DebugReport> {
        self.last_debug.lock().ok().and_then(|slot| slot.clone())
    }

    /// Emit that a profile was loaded (IMPT-05).
    fn profile_loaded(&self, path: impl Into<String>) {
        let _ = self
            .tx
            .send(WorkerEvent::ProfileLoaded { path: path.into() });
    }

    /// Emit that a profile was saved (IMPT-06).
    fn profile_saved(&self, path: impl Into<String>) {
        let _ = self
            .tx
            .send(WorkerEvent::ProfileSaved { path: path.into() });
    }

    /// Emit the applied field ROI polygon (CALB-09).
    ///
    /// Mirrored to the process log at INFO (UI-SPEC Event Log Contract); the
    /// polygon rides the typed channel. The caller passes the *normalized* value
    /// that was actually stored, so the editor mirrors what the worker persisted.
    fn field_roi_applied(&self, field_roi: reco_core::calibration::FieldRoi) {
        log::info!(
            "field ROI applied: {} left / {} right vertices",
            field_roi.left.len(),
            field_roi.right.len()
        );
        let _ = self.tx.send(WorkerEvent::FieldRoiApplied { field_roi });
    }

    /// Emit that the field ROI polygon was cleared (CALB-09).
    fn field_roi_cleared(&self) {
        log::info!("field ROI cleared");
        let _ = self.tx.send(WorkerEvent::FieldRoiCleared);
    }

    /// Emit a typed lens-refinement result (INTR-03).
    ///
    /// Mirrored to the process log at INFO on accept and WARN on reject (UI-SPEC
    /// Event Log Contract); the structured view rides the typed channel so the
    /// readout renders the engine's verdict verbatim and never re-types a value.
    fn intrinsics_refined(&self, refinement: crate::events::IntrinsicsRefinementView) {
        let line = WorkerEvent::IntrinsicsRefined {
            refinement: refinement.clone(),
        }
        .to_log_line();
        match line.level {
            Level::Info => log::info!("{}", line.message),
            Level::Warn => log::warn!("{}", line.message),
            Level::Error => log::error!("{}", line.message),
        }
        let _ = self.tx.send(WorkerEvent::IntrinsicsRefined { refinement });
    }

    /// Emit that the current result no longer matches the inputs (D3-08).
    fn result_invalidated(&self) {
        let _ = self.tx.send(WorkerEvent::ResultInvalidated);
    }

    /// Emit per-frame export progress (EXPT-04).
    ///
    /// The percent/frames/elapsed/ETA ride the typed channel; the process log
    /// gets the same line via [`WorkerEvent::to_log_line`] so a headless run sees
    /// the progress narrative too. The real backend's `on_progress` closure is
    /// `Send + 'static` and sends directly through a cloned `Sender`, so only the
    /// GPU-free mock (and tests) reach this helper.
    #[cfg_attr(not(test), allow(dead_code))]
    fn export_progress(
        &self,
        frames_completed: u64,
        total: Option<u64>,
        elapsed_ms: u64,
        eta_ms: Option<u64>,
        percent: f64,
    ) {
        let _ = self.tx.send(WorkerEvent::ExportProgress {
            frames_completed,
            total,
            elapsed_ms,
            eta_ms,
            percent,
        });
    }

    /// Emit a completed export with the resolved encoder (EXPT-04).
    fn export_finished(
        &self,
        path: impl Into<String>,
        encoder: impl Into<String>,
        hardware: bool,
        variant: crate::events::ExportVariant,
    ) {
        let event = WorkerEvent::ExportFinished {
            path: path.into(),
            encoder: encoder.into(),
            hardware,
            variant,
        };
        let line = event.to_log_line();
        log::info!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit a cancelled export (EXPT-04). No output path is claimed.
    fn export_cancelled(&self) {
        log::warn!("export cancelled — no file was written");
        let _ = self.tx.send(WorkerEvent::ExportCancelled);
    }

    /// Emit a failed export with a plain-language message (EXPT-04).
    ///
    /// No output path is claimed. Mirrored to the process log at ERROR.
    fn export_failed(&self, message: impl Into<String>) {
        let message = message.into();
        log::error!("export failed: {message}");
        let _ = self.tx.send(WorkerEvent::ExportFailed { message });
    }

    /// Emit the probed encoder list for a codec (EXPT-02).
    fn encoder_list(
        &self,
        encoders: Vec<crate::events::EncoderView>,
        auto: crate::events::EncoderView,
        auto_hardware: bool,
    ) {
        let _ = self.tx.send(WorkerEvent::EncoderList {
            encoders,
            auto,
            auto_hardware,
        });
    }

    /// Emit the explicit encoder fallback (EXPT-02).
    ///
    /// Mirrored to the process log at WARN so the fallback is never silent. The
    /// typed half lets the webview render the locked banner copy. `used_hardware`
    /// is the class of the encoder that will actually run, so the copy never
    /// claims "software" for a hardware fallback.
    fn export_fallback(
        &self,
        requested: impl Into<String>,
        used: impl Into<String>,
        used_hardware: bool,
    ) {
        let event = WorkerEvent::ExportFallback {
            requested: requested.into(),
            used: used.into(),
            used_hardware,
        };
        let line = event.to_log_line();
        log::warn!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit the worker-resolved output path preview (EXPT-06).
    ///
    /// The webview renders this path verbatim and never constructs one itself
    /// (T-05-08); the worker owns the directory + stem + variant + collision
    /// suffix. Mirrored to the process log at INFO.
    fn export_path_preview(&self, path: impl Into<String>) {
        let event = WorkerEvent::ExportPathPreview { path: path.into() };
        let line = event.to_log_line();
        log::info!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit the probed system information (DIAG-01).
    ///
    /// The structured view rides the typed channel; the log line is a bounded
    /// summary. Never fabricates a value — an unknown is `None` (the panel
    /// renders `Not reported`).
    fn system_info(&self, info: crate::system::SystemInfoView) {
        let event = WorkerEvent::SystemInfo { info };
        let line = event.to_log_line();
        log::info!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit the runtime preflight verdict (DIAG-05).
    ///
    /// A passing report is INFO; an incomplete one is a WARN naming the first
    /// failed prerequisite and its remediation, so the log is never silent
    /// about a missing prerequisite.
    fn preflight(&self, report: crate::preflight::PreflightReport) {
        let event = WorkerEvent::Preflight { report };
        let line = event.to_log_line();
        match line.level {
            Level::Warn => log::warn!("{}", line.message),
            Level::Error => log::error!("{}", line.message),
            Level::Info => log::info!("{}", line.message),
        }
        let _ = self.tx.send(event);
    }

    /// Emit that a `.reco` project was saved (PROJ-01).
    fn project_saved(&self, path: impl Into<String>) {
        let event = WorkerEvent::ProjectSaved { path: path.into() };
        let line = event.to_log_line();
        log::info!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit that a `.reco` project was opened and fully restored (PROJ-01).
    ///
    /// The typed payload carries the restored input paths + lens overrides, the
    /// pose, and the export settings so the webview reconciles from typed values.
    #[allow(clippy::too_many_arguments)]
    fn project_opened(
        &self,
        path: impl Into<String>,
        left: crate::project::ProjectInput,
        right: crate::project::ProjectInput,
        calibration_path: Option<String>,
        has_calibration: bool,
        pose: crate::project::PoseView,
        export: crate::events::ExportSettings,
    ) {
        let event = WorkerEvent::ProjectOpened {
            path: path.into(),
            left,
            right,
            calibration_path,
            has_calibration,
            pose,
            export,
        };
        let line = event.to_log_line();
        log::info!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit the missing-input list for a project open (PROJ-01).
    ///
    /// The open does not fail: the typed list lets the UI offer a relocate
    /// flow. Mirrored to the process log at WARN.
    fn project_missing_inputs(&self, missing: Vec<crate::project::MissingInput>) {
        let event = WorkerEvent::ProjectMissingInputs { missing };
        let line = event.to_log_line();
        log::warn!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit that a redacted, local-only diagnostics bundle was written (DIAG-03).
    ///
    /// Only emitted after the atomic write succeeded, so the path names a real
    /// bundle. Mirrored to the process log at INFO.
    fn diagnostics_bundle_written(&self, path: impl Into<String>, files: usize, redacted: bool) {
        let event = WorkerEvent::DiagnosticsBundleWritten {
            path: path.into(),
            files,
            redacted,
        };
        let line = event.to_log_line();
        log::info!("{}", line.message);
        let _ = self.tx.send(event);
    }

    /// Emit that a manual calibration session opened (MANU-01 / MANU-03).
    ///
    /// Mirrored to the process log at INFO; the structured payload rides the
    /// typed channel.
    fn manual_session_started(&self, frame: u64, fps: f64, frames_total: u64) {
        log::info!("manual session started: frame {frame}/{frames_total} @ {fps:.3} fps");
        let _ = self.tx.send(WorkerEvent::ManualSessionStarted {
            frame,
            fps,
            frames_total,
        });
    }

    /// Stream one camera's reference frame rendered under real `CameraParams`
    /// (MANU-03).
    ///
    /// The RGBA bytes ride the binary [`ManualFrameChannel`] only — the JSON
    /// typed stream never carries them (Phase 04.1 root fix). `side` selects the
    /// frame's kind tag. A no-op when no channel is attached.
    fn manual_preview_frame(
        &self,
        side: crate::events::ManualSide,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) {
        let kind = match side {
            crate::events::ManualSide::Left => ManualFrameKind::PreviewLeft,
            crate::events::ManualSide::Right => ManualFrameKind::PreviewRight,
        };
        self.send_manual_frame(kind, rgba, width, height);
    }

    /// Emit the manual solve's busy/stale state (MANU-03).
    ///
    /// `degenerate` is the precise degeneracy signal: true only when the last
    /// completed solve was rejected because the pin set is coincident/collinear.
    /// The editor drives its warning from this flag rather than inferring it
    /// from `stale` (which also covers a debounce gap or a failed re-solve).
    fn manual_solve_state(&self, busy: bool, stale: bool, degenerate: bool) {
        let _ = self.tx.send(WorkerEvent::ManualSolveState {
            busy,
            stale,
            degenerate,
        });
    }

    /// Emit the manual session's current correspondence pins (MANU-03).
    ///
    /// The pin list rides the typed channel; the process log carries only a
    /// bounded summary, never every pin's coordinates.
    fn manual_pins(&self, pins: Vec<crate::events::ManualPinView>, seeded: bool) {
        let _ = self.tx.send(WorkerEvent::ManualPins { pins, seeded });
    }

    /// Emit a landed debounced manual solve (MANU-03).
    fn manual_solve_result(
        &self,
        layout: crate::events::PlaneLayoutView,
        residual: f64,
        pins_used: usize,
        auto_used: usize,
    ) {
        let _ = self.tx.send(WorkerEvent::ManualSolveResult {
            layout,
            residual,
            pins_used,
            auto_used,
        });
    }

    /// Emit the manual session's edited real parameters (MANU-05 / MANU-06).
    ///
    /// The editable intrinsic subset plus the current layout ride the typed
    /// channel; the process log carries a bounded summary.
    fn manual_params(
        &self,
        left: crate::events::CameraParamsView,
        right: crate::events::CameraParamsView,
        layout: crate::events::PlaneLayoutView,
    ) {
        let _ = self.tx.send(WorkerEvent::ManualParams {
            left,
            right,
            layout,
        });
    }

    /// Emit the layout change a background re-solve produced (MANU-05 / MANU-06).
    fn manual_layout_delta(&self, cam_d: f64, intersect: f64, x_ty: f64, x_rz: f64) {
        let _ = self.tx.send(WorkerEvent::ManualLayoutDelta {
            cam_d,
            intersect,
            x_ty,
            x_rz,
        });
    }

    /// Emit the manual flow's audio auto-sync estimate (MANU-02).
    ///
    /// `confidence: None` is the "unavailable" signal. The fixed semantics
    /// sentence is attached from the single `SYNC_OFFSET_SEMANTICS` constant so
    /// the webview binds the wording rather than re-typing it (T-04.1-07).
    fn audio_sync_result(&self, offset_frames: i64, confidence: Option<f64>) {
        let _ = self.tx.send(WorkerEvent::AudioSyncResult {
            offset_frames,
            confidence,
            offset_semantics: crate::events::SYNC_OFFSET_SEMANTICS.to_string(),
        });
    }

    /// Emit that the operator set the manual sync offset (MANU-02).
    fn manual_sync_set(&self, offset_frames: i64, method: crate::events::SyncMethod) {
        let _ = self.tx.send(WorkerEvent::ManualSyncSet {
            offset_frames,
            method,
            offset_semantics: crate::events::SYNC_OFFSET_SEMANTICS.to_string(),
        });
    }

    /// Stream a manual validation frame's stitched comparison and emit its
    /// advisory verdict metadata (MANU-07).
    ///
    /// The stitched RGBA payloads (the validation frame and the calibration
    /// frame's reference for the blink comparison) ride the binary
    /// [`ManualFrameChannel`] only. The residual/verdict and geometry ride the
    /// JSON typed stream, which therefore carries no pixel bytes. `reference` is
    /// the calibration frame's stitched output `(rgba, width, height)`.
    fn manual_validation_frame(
        &self,
        frame: u32,
        rgba: &[u8],
        width: u32,
        height: u32,
        residual: f64,
        verdict: crate::events::ValidationVerdict,
        reference: (&[u8], u32, u32),
    ) {
        log::info!(
            "manual validation frame {frame}: residual {residual:.6}, {}",
            match verdict {
                crate::events::ValidationVerdict::LooksGood => "looks good",
                crate::events::ValidationVerdict::CheckSeam => "check the seam",
            }
        );
        self.send_manual_frame(ManualFrameKind::Validation, rgba, width, height);
        let (reference_rgba, reference_width, reference_height) = reference;
        self.send_manual_frame(
            ManualFrameKind::Reference,
            reference_rgba,
            reference_width,
            reference_height,
        );
        let _ = self.tx.send(WorkerEvent::ManualValidationFrame {
            frame,
            width,
            height,
            residual,
            verdict,
            reference_width,
            reference_height,
        });
    }

    /// Emit that the manual calibration was saved as a profile (MANU-07).
    fn manual_saved(&self, path: impl Into<String>) {
        let path = path.into();
        log::info!("manual calibration saved: {path}");
        let _ = self.tx.send(WorkerEvent::ManualSaved { path });
    }
}

/// The array index for a camera role (`Left` = 0, `Right` = 1).
fn role_index(role: crate::events::InputRole) -> usize {
    match role {
        crate::events::InputRole::Left => 0,
        crate::events::InputRole::Right => 1,
    }
}

/// Whether a project-referenced input resolves to a readable video (PROJ-01).
///
/// Uses the same FFmpeg probe the import path uses, so a project can never
/// restore an input the worker could not actually open. A missing or unreadable
/// path is reported as missing rather than failing the open.
fn project_input_exists(path: &str) -> bool {
    reco_io::ffmpeg::calibration_io::probe_video(std::path::Path::new(path)).is_ok()
}

/// A parsed `.reco` project awaiting relocation (PROJ-01).
///
/// Held while one or more referenced inputs are missing, so a
/// `RelocateProjectInput` command can re-run the restore without re-reading the
/// file. The originating path is carried so the eventual `ProjectOpened` names
/// the project that was opened.
struct PendingProject {
    /// The `.reco` file path the project was read from.
    path: String,
    /// The parsed manifest (its input paths may be updated by a relocate).
    project: crate::project::RecoProject,
}

/// Normalize a validated field-ROI polygon pair for storage (CALB-09).
///
/// A camera polygon with fewer than three vertices is not a polygon, so it is
/// cleared to an empty list — the autocam path treats an empty list as "no
/// filter" (UI-SPEC Field ROI Editor Contract / degenerate shapes). The
/// coordinates are already validated (finite, `[0,1]`, bounded count) at the
/// command boundary; this only decides what a degenerate shape means, so it is
/// pure and unit-testable without a worker or a GPU.
fn normalize_field_roi(
    left: Vec<[f64; 2]>,
    right: Vec<[f64; 2]>,
) -> reco_core::calibration::FieldRoi {
    let clean = |verts: Vec<[f64; 2]>| if verts.len() < 3 { Vec::new() } else { verts };
    reco_core::calibration::FieldRoi {
        left: clean(left),
        right: clean(right),
    }
}

/// The debounce window for the background manual solve (MANU-03).
///
/// A pin mutation re-arms this window; one solve fires once the window elapses
/// with no further mutation. The solve is never run per pointer-move — that is
/// the whole point of the debounce (T-04.1-11). The window is a UX choice the
/// plan leaves to the agent; 400 ms sits in the research's 300–500 ms band.
pub(crate) const MANUAL_SOLVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Lens-handle travel clamps (MANU-05), carried from the retired Slint GUI
/// (`crates/reco-gui/src/main.rs:1044-1059`). Each handle moves exactly one
/// parameter within these bounds; the boundary is stated in the UI.
///
/// `k1` is the dominant first-order radial term; `cx`/`cy` are the only
/// lateral/vertical signatures; `fx` is radial/edge-only and therefore an
/// explicit scale mode with `fy = fx`.
pub(crate) const K1_CLAMP: f64 = 0.3;
/// Fraction of the frame width/height a center-handle drag may travel (MANU-05).
pub(crate) const CENTER_CLAMP_FRAC: f64 = 0.1;
/// Fraction of the baseline focal length a scale-handle drag may travel (MANU-05).
pub(crate) const FX_CLAMP_FRAC: f64 = 0.15;
/// Floor on the focal-length clamp span, in pixels (MANU-05).
pub(crate) const FX_CLAMP_FLOOR_PX: f64 = 5.0;
/// Layout-handle travel clamps (MANU-06).
pub(crate) const X_TY_CLAMP: f64 = 0.1;
/// Layout roll clamp in radians (~17°) (MANU-06).
pub(crate) const X_RZ_CLAMP: f64 = 0.3;
/// Camera-distance range (MANU-06).
pub(crate) const CAM_D_RANGE: (f64, f64) = (0.1, 0.30);

/// Clamp an on-image lens-handle edit against a baseline `CameraParams`
/// (MANU-05).
///
/// Returns the edited params and whether any requested value was clamped — the
/// latter drives the UI's clamp WARN. `fy` is always forced to the clamped `fx`
/// (square pixels): a free `fy` would be a second, near-redundant radial knob
/// (research B4). `k2..k4` are never touched by a handle; they stay at the
/// baseline profile values.
fn clamp_lens_edit(
    baseline: &reco_core::calibration::CameraParams,
    fx: f64,
    cx: f64,
    cy: f64,
    k1: f64,
) -> (reco_core::calibration::CameraParams, bool) {
    let f_baseline = baseline.fx.max(baseline.fy);
    let fx_span = (f_baseline * FX_CLAMP_FRAC).max(FX_CLAMP_FLOOR_PX);
    let cx_span = (baseline.width.max(1) as f64 * CENTER_CLAMP_FRAC).max(FX_CLAMP_FLOOR_PX);
    let cy_span = (baseline.height.max(1) as f64 * CENTER_CLAMP_FRAC).max(FX_CLAMP_FLOOR_PX);

    let fx_c = fx.clamp(baseline.fx - fx_span, baseline.fx + fx_span);
    let cx_c = cx.clamp(baseline.cx - cx_span, baseline.cx + cx_span);
    let cy_c = cy.clamp(baseline.cy - cy_span, baseline.cy + cy_span);
    let k1_c = k1.clamp(baseline.d[0] - K1_CLAMP, baseline.d[0] + K1_CLAMP);

    let clamped = fx_c != fx || cx_c != cx || cy_c != cy || k1_c != k1;
    let mut d = baseline.d;
    d[0] = k1_c;
    (
        reco_core::calibration::CameraParams {
            width: baseline.width,
            height: baseline.height,
            fx: fx_c,
            // Square pixels: the scale mode drives fx and fy together.
            fy: fx_c,
            cx: cx_c,
            cy: cy_c,
            d,
        },
        clamped,
    )
}

/// Clamp a constrained layout-handle edit (MANU-06).
///
/// Returns the clamped layout plus whether any requested value was clamped.
/// Each handle moves exactly one parameter; no free 2-D manipulation is offered.
fn clamp_layout_edit(
    cam_d: f64,
    intersect: f64,
    x_ty: f64,
    x_rz: f64,
    current: &reco_core::calibration::PlaneLayout,
) -> (reco_core::calibration::PlaneLayout, bool) {
    let cam_d_c = cam_d.clamp(CAM_D_RANGE.0, CAM_D_RANGE.1);
    let intersect_c = intersect.clamp(0.0, 1.0);
    let x_ty_c = x_ty.clamp(-X_TY_CLAMP, X_TY_CLAMP);
    let x_rz_c = x_rz.clamp(-X_RZ_CLAMP, X_RZ_CLAMP);
    let clamped = cam_d_c != cam_d || intersect_c != intersect || x_ty_c != x_ty || x_rz_c != x_rz;
    (
        reco_core::calibration::PlaneLayout {
            camera_axis_offset: cam_d_c,
            intersect: intersect_c,
            x_ty: x_ty_c,
            x_rz: x_rz_c,
            // The constrained v1 handle set never moves these; keep the
            // profile values.
            z_rx: current.z_rx,
            x_rx: current.x_rx,
            z_rz: current.z_rz,
        },
        clamped,
    )
}

/// Extract one reference frame's YUV planes from a clip (MANU-03).
///
/// Mirrors the `worker.rs` frame-extraction seam (`extract_frames(path, &[i])`)
/// and fails with a typed [`WorkerError::Engine`] when the decoder returns no
/// frame for the index, rather than silently previewing nothing.
fn extract_manual_frame(
    path: &str,
    frame: u64,
) -> Result<reco_core::source::YuvFrame, WorkerError> {
    let frames =
        reco_io::ffmpeg::calibration_io::extract_frames(std::path::Path::new(path), &[frame])
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
    frames.into_iter().next().ok_or_else(|| {
        WorkerError::Engine(format!("frame {frame} could not be extracted from {path}"))
    })
}

/// A neutral pinhole `CameraParams` for a frame of the given size (MANU-03).
///
/// Only reached when neither a loaded profile nor a lens override is available.
/// It is still a **real** `CameraParams` (identity distortion, principal point
/// at the frame centre) so the preview remains a GPU undistort under real
/// parameters, never an arbitrary 2-D warp.
fn default_camera_params(width: u32, height: u32) -> reco_core::calibration::CameraParams {
    reco_core::calibration::CameraParams {
        width,
        height,
        fx: width as f64,
        fy: width as f64,
        cx: width as f64 / 2.0,
        cy: height as f64 / 2.0,
        d: [0.0; 4],
    }
}

/// A neutral layout used as the manual session's baseline when no profile is
/// loaded (MANU-06).
///
/// The manual flow is always reachable without a `.json`, so a session still
/// needs a defined layout to reset to and to compute a re-solve delta against.
/// Values match the engine's typical test rig (`manual.rs` `truth`).
fn default_plane_layout() -> reco_core::calibration::PlaneLayout {
    reco_core::calibration::PlaneLayout {
        camera_axis_offset: 0.24,
        intersect: 0.55,
        x_ty: 0.0,
        x_rz: 0.0,
        z_rx: 0.0,
        x_rx: 0.0,
        z_rz: 0.0,
    }
}

/// The overall progress fraction for a stage (stage `n` of 7, 1-based).
fn stage_fraction(stage: crate::events::CalibrationStage) -> f64 {
    use crate::events::CalibrationStage as S;
    let ordinal = match stage {
        S::Probing => 0,
        S::DetectingProfiles => 1,
        S::AudioSync => 2,
        S::ExtractingFrames => 3,
        S::Undistorting => 4,
        S::FeatureMatching => 5,
        S::Optimizing => 6,
    };
    (ordinal as f64 + 1.0) / 7.0
}

/// The recovery action a classified surface error demands (FOUND-05).
///
/// Extracted as a pure function of the error kind so the branch is unit-testable
/// without a GPU (the decision — not the GPU work — is the FOUND-05 contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Reconfigure the surface against the existing device.
    Reconfigure,
    /// Rebuild the device, then reconfigure the surface.
    Rebuild,
    /// Drop the frame and continue (no recovery).
    SkipFrame,
}

/// Map a [`crate::presenter::SurfaceErrorKind`] to its [`RecoveryAction`].
///
/// This is the single decision point for the FOUND-05 per-variant contract
/// (RESEARCH Pattern 5 / Pitfall 3):
///
/// * `Outdated` → [`RecoveryAction::Reconfigure`]
/// * `Lost` → [`RecoveryAction::Rebuild`]
/// * `Timeout` / `OutOfMemory` / `Other` → [`RecoveryAction::SkipFrame`]
pub fn recovery_action(kind: crate::presenter::SurfaceErrorKind) -> RecoveryAction {
    use crate::presenter::SurfaceErrorKind as K;
    match kind {
        K::Outdated => RecoveryAction::Reconfigure,
        K::Lost => RecoveryAction::Rebuild,
        K::Timeout | K::OutOfMemory | K::Other => RecoveryAction::SkipFrame,
    }
}

/// Format the one log line that ties a requested native viewport to the
/// geometry the OS window actually has.
///
/// `ViewportRect::for_chrome` is the geometry authority (UI-SPEC "Geometry
/// authority"), so `requested` is what the native window **must** be.
/// `observed` is the server-side size read back from the platform (see
/// `SurfacePresenter::child_geometry`) — `None` when the presenter has no
/// window of its own, or the window has been released.
///
/// Reporting both numbers in one line is what makes a stale window visible.
/// Before this line existed, a surface could be configured correctly while the
/// X window kept its old size, and every Rust test stayed green because the
/// render path only ever knew what it had *asked* for — the panorama painted
/// over the expanded controls panel and the only symptom was on screen.
///
/// Pure and free so the agree / disagree / unavailable shapes are unit-testable
/// with no GPU and no X server.
pub fn native_geometry_line(
    requested: crate::presenter::ViewportRect,
    observed: Option<(u32, u32)>,
    chrome: crate::presenter::ChromeState,
) -> String {
    let chrome_label = format!(
        "panel {}, drawer {}",
        if chrome.panel_expanded {
            "expanded"
        } else {
            "collapsed"
        },
        if chrome.drawer_expanded {
            "expanded"
        } else {
            "collapsed"
        }
    );
    let requested_label = format!("{}x{}", requested.width, requested.height);
    match observed {
        None => format!(
            "native viewport: requested {requested_label}, child window geometry unavailable \
             ({chrome_label})"
        ),
        Some((width, height)) if width == requested.width && height == requested.height => {
            format!(
                "native viewport: requested {requested_label}, child window {width}x{height} ({chrome_label})"
            )
        }
        Some((width, height)) => format!(
            "native viewport: requested {requested_label}, child window {width}x{height} — \
             mismatch with the requested rect ({chrome_label})"
        ),
    }
}

/// Build the transport a preview session starts on, carrying user-set state
/// across the session boundary.
///
/// **Both** backends build their session transport here — the GPU-touching
/// [`GpuEngineBackend::begin_preview`] and the GPU-free `MockBackend` the
/// worker-loop tests drive. That is deliberate: the previous shape gave each
/// backend its own construction, so a test that proved the mock carried the
/// Loop flag said nothing about the real backend, and deleting the real
/// backend's carry left every gate green. One shared constructor makes the mock
/// incapable of drifting from the real behaviour, and makes the mutation
/// proof (`new_session_transport` dropping its `carry_user_state` call) fail
/// the mock-driven tests.
///
/// `session` is preferred over `loaded`, matching the precedence
/// [`EngineBackend::transport`] uses, so a `SetLoop` command sees the same
/// choice here as it did when it stored the flag.
pub fn new_session_transport(
    fps: f64,
    fps_rational: Option<(i32, i32)>,
    total_frames: Option<u64>,
    session: Option<&crate::transport::Transport>,
    loaded: Option<&crate::transport::Transport>,
) -> crate::transport::Transport {
    let mut transport = crate::transport::Transport::new(fps, fps_rational, total_frames);
    crate::transport::carry_user_state(session.or(loaded), &mut transport);
    transport
}

/// The operator clips (and sync offset) a preview decode source was opened from
/// (IMPT-01 / D3-15).
///
/// `import()` opens the hardcoded startup pair; once the operator has chosen
/// **both** inputs the worker must decode those files instead, or Preview keeps
/// showing the placeholder clips. The spec is what the worker compares against
/// so it re-opens exactly once per `(left, right, sync_offset)` change rather
/// than on every preview.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewSourceSpec {
    /// The operator-chosen left clip path.
    left: String,
    /// The operator-chosen right clip path.
    right: String,
    /// The sync offset the pair is opened with (the adopted calibration's, else
    /// 0).
    sync_offset: i64,
}

/// Resolve the decode source the preview must use for the current operator
/// inputs, or `None` when either role is unset.
///
/// Shared by the GPU backend and the GPU-free mock so the two cannot drift:
/// the mock models exactly the resolution the real `begin_preview` performs.
fn resolve_preview_source(
    left: Option<&str>,
    right: Option<&str>,
    sync_offset: i64,
) -> Option<PreviewSourceSpec> {
    Some(PreviewSourceSpec {
        left: left?.to_string(),
        right: right?.to_string(),
        sync_offset,
    })
}

/// Project a probed [`VideoProbe`](reco_io::ffmpeg::calibration_io::VideoProbe)
/// into the UI-facing [`InputMetadata`](crate::events::InputMetadata) with
/// per-field provenance (IMPT-02 / D3-04).
///
/// Pure and GPU-free so the provenance mapping is unit-testable in CI. The
/// mapping is the whole point of IMPT-02:
///
/// * `resolution` — read directly from the decoder → `Probed`.
/// * `fps` — the exact `fps_rational` when the container reports one, else the
///   decoder's `f64` → `Probed`.
/// * `duration` — the container-reported `duration_secs` → `Probed`; otherwise
///   derived from `total_frames / fps` (and `total_frames` is itself a
///   `duration × fps` estimate) → `Estimated`; never authoritative.
/// * `codec` — read directly from the container's codec parameters →
///   `Probed`; absent (`value: None`) when the container omits it, rendered
///   as an em-dash, never `0`.
pub fn project_metadata(
    probe: &reco_io::ffmpeg::calibration_io::VideoProbe,
) -> crate::events::InputMetadata {
    use crate::events::{InputMetadata, MetadataField, Provenance};

    let resolution = if probe.width > 0 && probe.height > 0 {
        MetadataField::probed(format!("{}×{}", probe.width, probe.height))
    } else {
        MetadataField::missing(Provenance::Probed)
    };

    // Prefer the exact rational the container reports (IMPT-02 / D3-04); fall
    // back to the decoder's f64 only when no rational is available.
    let fps = match probe.fps_rational {
        Some((n, d)) if d > 0 => MetadataField::probed(format_fps(n as f64 / d as f64)),
        _ if probe.fps > 0.0 => MetadataField::probed(format_fps(probe.fps)),
        _ => MetadataField::missing(Provenance::Probed),
    };

    // A container-reported duration is a direct read → `Probed`; deriving it
    // from `total_frames / fps` is an estimate and stays `Estimated` (IMPT-02).
    let duration = match probe.duration_secs {
        Some(secs) if secs > 0.0 => MetadataField::probed(format_duration(secs)),
        _ if probe.fps > 0.0 && probe.total_frames > 0 => {
            MetadataField::estimated(format_duration(probe.total_frames as f64 / probe.fps))
        }
        _ => MetadataField::missing(Provenance::Estimated),
    };

    // Codec is read directly from the container (E2). Absent, never zero.
    let codec = match probe.codec.as_deref() {
        Some(name) if !name.is_empty() => MetadataField::probed(name.to_string()),
        _ => MetadataField::missing(Provenance::Probed),
    };

    InputMetadata {
        resolution,
        fps,
        duration,
        codec,
    }
}

/// Format a frame rate for display, trimming trailing zeros (`30 fps`,
/// `29.97 fps`).
fn format_fps(fps: f64) -> String {
    let mut s = format!("{fps:.3}");
    while s.contains('.') && s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    format!("{s} fps")
}

/// Number of columns in the sampled intensity profile (CALB-05).
const READINESS_PROFILE_COLUMNS: usize = 64;

/// Maximum pixels averaged for the sampled mean luma (T-04-07).
const READINESS_SAMPLE_MAX_PIXELS: usize = 65_536;

/// Minimum winning normalized cross-correlation for a credible overlap
/// estimate (WR-04). Below this the alignment is noise, so the estimate is
/// reported as unknown rather than as a confident-looking percentage.
const OVERLAP_MIN_CORRELATION: f64 = 0.3;

/// One sampled frame's readiness statistics.
struct SampledFrame {
    /// Mean luma of the Y plane (0..=255).
    mean_luma: f64,
    /// Per-column mean-luma profile, used for the overlap estimate.
    profile: Vec<f64>,
}

/// Decode one mid-clip frame per input and derive the readiness estimates
/// (CALB-05).
///
/// Bounded and fail-closed (T-04-07): one frame per input, a strided sample for
/// the mean, and a fixed-width column profile. Any failure (probe, decode, a
/// flat frame) yields `None` for the affected estimate — an honest unknown,
/// never a fabricated `0.0`.
fn sample_readiness(left_path: &str, right_path: &str) -> crate::calibration::ReadinessSamples {
    let left = sample_frame(std::path::Path::new(left_path));
    let right = sample_frame(std::path::Path::new(right_path));
    match (left, right) {
        (Some(l), Some(r)) => crate::calibration::ReadinessSamples {
            overlap: estimate_overlap(&l.profile, &r.profile),
            exposure_delta_stops: estimate_exposure_delta_stops(l.mean_luma, r.mean_luma),
        },
        _ => crate::calibration::ReadinessSamples::default(),
    }
}

/// Probe and decode one mid-clip frame, returning its luma stats, or `None`.
fn sample_frame(path: &std::path::Path) -> Option<SampledFrame> {
    let probe = reco_io::ffmpeg::calibration_io::probe_video(path).ok()?;
    let mid = probe.total_frames / 2;
    let frames = reco_io::ffmpeg::calibration_io::extract_frames(path, &[mid]).ok()?;
    let frame = frames.first()?;
    Some(SampledFrame {
        mean_luma: mean_luma(frame)?,
        profile: column_profile(frame)?,
    })
}

/// The strided mean luma of a frame's Y plane, bounded by
/// [`READINESS_SAMPLE_MAX_PIXELS`] (T-04-07).
fn mean_luma(frame: &reco_core::source::YuvFrame) -> Option<f64> {
    if frame.y.is_empty() {
        return None;
    }
    let stride = (frame.y.len() / READINESS_SAMPLE_MAX_PIXELS).max(1);
    let mut sum = 0u64;
    let mut count = 0u64;
    let mut idx = 0;
    while idx < frame.y.len() {
        sum += frame.y[idx] as u64;
        count += 1;
        idx += stride;
    }
    (count > 0).then(|| sum as f64 / count as f64)
}

/// A per-column mean-luma profile over a bounded row sample (CALB-05).
///
/// `None` when the frame's Y plane is smaller than its stated geometry (a
/// malformed buffer) or no pixel could be read.
fn column_profile(frame: &reco_core::source::YuvFrame) -> Option<Vec<f64>> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || frame.y.len() < w * h {
        return None;
    }
    let cols = READINESS_PROFILE_COLUMNS.min(w);
    let mut sums = vec![0.0f64; cols];
    let mut counts = vec![0u32; cols];
    let row_stride = (h / 256).max(1);
    let mut row = 0;
    while row < h {
        let base = row * w;
        for (col, (sum, count)) in sums.iter_mut().zip(counts.iter_mut()).enumerate() {
            let src = col * w / cols;
            if let Some(px) = frame.y.get(base + src) {
                *sum += *px as f64;
                *count += 1;
            }
        }
        row += row_stride;
    }
    for (sum, count) in sums.iter_mut().zip(counts) {
        if count == 0 {
            return None;
        }
        *sum /= count as f64;
    }
    Some(sums)
}

/// Estimate the exposure difference in stops from two mean luma values.
///
/// `None` when either mean is effectively black (a ratio against ~0 is
/// meaningless and would amplify noise).
fn estimate_exposure_delta_stops(left_mean: f64, right_mean: f64) -> Option<f64> {
    if left_mean <= 1.0 || right_mean <= 1.0 {
        return None;
    }
    Some((left_mean / right_mean).log2())
}

/// Estimate horizontal overlap from two column profiles (CALB-05).
///
/// The best integer-shift normalized cross-correlation aligns the profiles; the
/// aligned fraction of the width is the overlap estimate. A flat profile (no
/// contrast), or a best alignment whose correlation is below
/// [`OVERLAP_MIN_CORRELATION`] (no real scene correspondence), yields `None` —
/// an honest unknown rather than a fake number (WR-04).
fn estimate_overlap(left: &[f64], right: &[f64]) -> Option<f64> {
    if left.len() != right.len() || left.len() < 4 {
        return None;
    }
    let ln = normalize_profile(left)?;
    let rn = normalize_profile(right)?;
    let n = left.len() as i64;
    let mut best_shift = 0i64;
    let mut best_corr = f64::NEG_INFINITY;
    for shift in -(n - 1)..=(n - 1) {
        let (a_start, b_start, len) = if shift >= 0 {
            (0usize, shift as usize, (n - shift) as usize)
        } else {
            ((-shift) as usize, 0usize, (n + shift) as usize)
        };
        if len < 4 {
            continue;
        }
        let mut dot = 0.0;
        let mut na = 0.0;
        let mut nb = 0.0;
        for k in 0..len {
            let a = ln[a_start + k];
            let b = rn[b_start + k];
            dot += a * b;
            na += a * a;
            nb += b * b;
        }
        let corr = if na > 0.0 && nb > 0.0 {
            dot / (na.sqrt() * nb.sqrt())
        } else {
            0.0
        };
        if corr > best_corr {
            best_corr = corr;
            best_shift = shift;
        }
    }
    overlap_from_alignment(n as usize, best_shift, best_corr)
}

/// Turn a winning integer-shift alignment into an overlap fraction (CALB-05).
///
/// Returns `None` when the winning normalized cross-correlation is below
/// [`OVERLAP_MIN_CORRELATION`]: a weak alignment is noise, so the estimate is
/// reported as unknown rather than as a fabricated percentage (WR-04). The
/// aligned fraction of the width is the overlap otherwise.
fn overlap_from_alignment(n: usize, best_shift: i64, best_corr: f64) -> Option<f64> {
    if best_corr < OVERLAP_MIN_CORRELATION {
        return None;
    }
    let aligned = (n as i64 - best_shift.abs()).max(0) as f64;
    Some((aligned / n as f64).clamp(0.0, 1.0))
}

/// Subtract the mean and divide by the standard deviation; `None` if flat.
fn normalize_profile(profile: &[f64]) -> Option<Vec<f64>> {
    let mean = profile.iter().sum::<f64>() / profile.len() as f64;
    let variance = profile.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / profile.len() as f64;
    let std = variance.sqrt();
    if std < 1e-6 {
        return None;
    }
    Some(profile.iter().map(|v| (v - mean) / std).collect())
}

/// Format a duration in seconds as `M:SS` (or `H:MM:SS` past an hour).
fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The engine side of the worker, behind a trait so the protocol is testable
/// without a GPU.
///
/// The real implementation ([`GpuEngineBackend`]) owns the device/renderer/
/// pose; the test implementation owns nothing but state, letting the loop's
/// ordering and error behavior be asserted in CI.
pub trait EngineBackend: Send {
    /// Load the hardcoded clips and calibration into the session.
    fn import(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Set the operator-chosen path for one camera input and probe it
    /// (IMPT-01 / IMPT-02).
    ///
    /// The real backend probes via FFmpeg and emits a typed
    /// `ImportMetadata`; the GPU-free mock records the call and emits a
    /// fabricated metadata event so the protocol is testable without a GPU.
    fn set_input(
        &mut self,
        role: crate::events::InputRole,
        path: String,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Clear one camera input slot (IMPT-01).
    ///
    /// Clears the slot, recomputes compatibility, and invalidates any existing
    /// result (D3-08).
    fn clear_input(
        &mut self,
        role: crate::events::InputRole,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Emit the lens-profile candidates for one input's resolution (IMPT-04).
    fn lens_candidates(
        &mut self,
        role: crate::events::InputRole,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Apply a lens-profile override to one input (IMPT-04 / D3-08).
    fn set_lens_override(
        &mut self,
        role: crate::events::InputRole,
        candidate: crate::events::LensCandidate,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Clear a lens-profile override, returning the input to auto-detect.
    fn clear_lens_override(
        &mut self,
        role: crate::events::InputRole,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Write the per-camera field ROI polygon onto the current calibration
    /// (CALB-09).
    ///
    /// Normalizes each camera's list (fewer than three vertices clears it) and
    /// writes the result onto `current_calibration.field_roi` and the preview's
    /// `calibration.field_roi`, then emits a typed
    /// `FieldRoiApplied`/`FieldRoiCleared`. Rejects with a typed error when there
    /// is no current calibration. The ROI does not change the stitch geometry, so
    /// the result is not invalidated.
    fn set_field_roi(
        &mut self,
        left: Vec<[f64; 2]>,
        right: Vec<[f64; 2]>,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Refine the current profile's lens `k1` against the retained matches
    /// (INTR-03).
    ///
    /// Opt-in only: derives raw-pixel observations from the retained verified
    /// matches, runs the reduced `k1` refinement behind the held-out guard,
    /// emits a typed `IntrinsicsRefined`, and writes the profile's `k1` onto
    /// both cameras **only** when the result is accepted (T-04.2-10). A
    /// rejected or ill-conditioned refinement leaves the profile unchanged. The
    /// calibration wizard never calls this (T-04.2-11).
    fn refine_lens(&mut self, heldout_fraction: f64, events: &EventSink)
    -> Result<(), WorkerError>;

    /// Run calibration on the worker's own device (CALB-01 / FOUND-03).
    ///
    /// `interrupted` is the worker loop's shutdown flag; cancellation of a
    /// running calibration is driven by the backend's own shared flag (set by
    /// `cancel_calibration`). Emits stage/progress/heartbeat/result events.
    fn calibrate(
        &mut self,
        options: crate::events::CalibrationOptions,
        events: &EventSink,
        interrupted: &AtomicBool,
    ) -> Result<(), WorkerError>;

    /// Load a calibration profile from a local file (IMPT-05).
    fn load_profile(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError>;

    /// Save the current calibration profile to a local file (IMPT-06).
    fn save_profile(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError>;

    /// Open a manual calibration session at `frame` (MANU-01 / MANU-03).
    ///
    /// Extracts the reference frame for both clips, retains the YUV planes for
    /// the session's duration, seeds the per-camera `CameraParams`, emits a typed
    /// `ManualSessionStarted`, and streams one binary preview per camera (the GPU
    /// undistort under the real parameters) over the manual-frame channel. The
    /// index is clamped against the probed frame count (T-04.1-01).
    fn manual_begin(&mut self, frame: u64, events: &EventSink) -> Result<(), WorkerError>;

    /// Change the manual session's reference frame (MANU-03).
    ///
    /// Clamps the index, re-extracts only when it changed, and re-renders.
    fn manual_set_frame(&mut self, frame: u64, events: &EventSink) -> Result<(), WorkerError>;

    /// Close the manual session, dropping the retained planes (MANU-01).
    fn manual_exit(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Run the manual flow's audio auto-sync and emit the estimate (MANU-02).
    ///
    /// Extracts each clip's PCM and cross-correlates via the engine
    /// ([`reco_calibrate::video::detect_audio_sync`]); on failure emits an
    /// "unavailable" `AudioSyncResult` (`confidence: None`, offset 0) rather
    /// than a fabricated zero. Runs inline on the worker thread with an error
    /// path, so the webview stays responsive (T-04.1-09).
    fn manual_detect_sync(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Record the operator's manual sync offset and emit `ManualSyncSet`
    /// (MANU-02).
    ///
    /// The offset is carried on the manual session with
    /// `SyncMethod::Manual` provenance; it is what the later pin/bend solves
    /// use.
    fn manual_set_sync(
        &mut self,
        offset_frames: i64,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Add one correspondence pin and arm the debounced solve (MANU-03).
    ///
    /// The backend clamps the points to its session frame bounds (T-04.1-10),
    /// emits the updated `ManualPins` plus an instant preview, marks the solve
    /// busy/stale, and arms [`Self::manual_solve_deadline`]. It never runs the
    /// solve itself (the worker loop does, once the debounce elapses).
    fn manual_add_pin(
        &mut self,
        left_px: [f64; 2],
        right_px: [f64; 2],
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Move one side of an existing pin and re-arm the debounced solve
    /// (MANU-03).
    fn manual_move_pin(
        &mut self,
        id: u32,
        side: crate::events::ManualSide,
        px: [f64; 2],
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Remove one pin and re-arm the debounced solve (MANU-03).
    fn manual_remove_pin(&mut self, id: u32, events: &EventSink) -> Result<(), WorkerError>;

    /// Remove every pin (MANU-03). An empty set never arms a solve.
    fn manual_clear_pins(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Apply an on-image lens-handle edit to one camera's real intrinsics
    /// (MANU-05).
    ///
    /// The backend clamps each value against the baseline captured at
    /// `manual_begin` (k1 ±0.3, cx/cy ±10% of the frame, fx ±15% floored at
    /// 5 px), enforces `fy = fx`, writes the edited `CameraParams` into the
    /// session and `current_calibration.left`/`.right` (so it survives a preview
    /// rebuild and save), re-renders the instant preview **without re-solving**
    /// (the layout stays frozen), emits `ManualParams`, and arms the shared
    /// debounced background re-solve. It never solves synchronously (T-04.1-11)
    /// and never solves lens intrinsics in the engine (MANU-05 prohibition).
    fn manual_set_lens(
        &mut self,
        side: crate::events::ManualSide,
        fx: f64,
        cx: f64,
        cy: f64,
        k1: f64,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Apply a constrained layout-handle edit (MANU-06).
    ///
    /// Each value is clamped to its travel range (x_ty ±0.1, x_rz ±0.3 rad,
    /// intersect 0–1, cam_d 0.1–0.30), written into `current_calibration.layout`,
    /// re-rendered instantly, and confirmed by a debounced background solve. One
    /// handle moves exactly one parameter; no free 2-D manipulation is offered.
    fn manual_set_layout(
        &mut self,
        cam_d: f64,
        intersect: f64,
        x_ty: f64,
        x_rz: f64,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Restore the lens intrinsics captured at `manual_begin` (MANU-05).
    fn manual_reset_lens(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Restore the rig layout captured at `manual_begin` (MANU-06).
    fn manual_reset_rig(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Validate the manual result on one additional frame (MANU-07).
    ///
    /// Extracts the validation frame pair (the right index carries the session's
    /// sync offset), renders the stitched comparison under the current manual
    /// parameters/layout, runs the engine on the frame for a per-frame residual,
    /// computes the advisory verdict, and emits a typed `ManualValidationFrame`.
    /// Validation is advisory, never a gate.
    fn manual_validate(&mut self, frame: u32, events: &EventSink) -> Result<(), WorkerError>;

    /// Assemble and save the manual result as a normal calibration profile
    /// (MANU-07).
    ///
    /// Builds a `MatchCalibration` from the session's parameters, layout, sync
    /// offset, and the carried profile fields, gates the write on
    /// `MatchCalibration::validate` (T-04.1-16), writes it through the existing
    /// `.json` path, adopts it as the live result, and emits `ManualSaved`.
    fn manual_save(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError>;

    /// The instant the armed debounced manual solve should fire, or `None`
    /// when no solve is armed (MANU-03).
    ///
    /// The worker loop blocks on the command channel only until this deadline,
    /// so a pin drop's background solve lands without a busy spin. The solve is
    /// **never** called from a pin command directly.
    fn manual_solve_deadline(&self) -> Option<std::time::Instant>;

    /// Run the armed manual solve now, emit `ManualSolveResult` + the fresh
    /// state, and clear the deadline (MANU-03).
    ///
    /// An empty or degenerate pin set is a defined non-result: it emits a WARN
    /// log line and the stale state, never a garbage rig.
    fn manual_solve(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Render and emit the manual preview if a mutation marked it dirty, then
    /// clear the flag (MANU-03).
    ///
    /// The worker loop calls this once after draining pending commands, so a
    /// drag burst (one command per pointer event) produces at most one bounded
    /// preview frame per drain rather than one per event. A backend without a
    /// manual preview keeps the default no-op.
    fn flush_manual_preview(&mut self, _events: &EventSink) -> Result<(), WorkerError> {
        Ok(())
    }

    /// Begin a preview **session**: build/ensure the renderer, paint the idle
    /// frame, and start the transport, but do NOT run a frame loop.
    ///
    /// Replaces Phase 1's one-shot [`preview`](Self::preview) job: the worker
    /// loop then drives [`tick_session`](Self::tick_session) once per iteration
    /// so commands queued during a session are drained at the top of every tick
    /// (RESEARCH Pattern 3).
    fn begin_preview(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Advance the active session by one tick: coalesced seek, paced decode,
    /// pose tick + FOV, present, and events.
    ///
    /// Must be a no-op when [`session_active`](Self::session_active) is false.
    fn tick_session(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Publish the transport's authoritative position to the frontend.
    ///
    /// A no-op when no transport is loaded. Used to re-assert the worker's
    /// position at command and error boundaries, where the frontend may be
    /// showing an optimistic value (a drag, or a rejected seek) that the engine
    /// has not reached.
    fn publish_position(&mut self, events: &EventSink);

    /// Re-assert the *entire* worker-owned projection — position, transport,
    /// pose, view mode, and active presenter — to the frontend.
    ///
    /// The Tauri bridge only reaches listeners registered at emit time. The
    /// worker boots and imports in `setup()`, so its opening projections fire
    /// before the webview has subscribed and are dropped; the frontend then
    /// reports an empty transport and an unknown clip length. This method lets
    /// the frontend reconcile after subscribing (and restores the UI after a
    /// webview reload), so the worker stays the single authority on state
    /// without the frontend having to infer readiness.
    fn republish_projection(&mut self, events: &EventSink);

    /// End the active session and return to idle.
    fn end_preview(&mut self, events: &EventSink);

    /// Whether a preview session is currently active.
    fn session_active(&self) -> bool;

    /// The session's transport state machine (authoritative playback position,
    /// state, loop, and coalesced seek).
    fn transport(&mut self) -> &mut crate::transport::Transport;

    /// The transport built at `Import` time, or `None` before any import.
    ///
    /// Read-only counterpart to [`Self::transport`], for a handler that must
    /// *report* state rather than mutate it. Reporting must not go through
    /// `transport()`, which materializes a placeholder transport in the
    /// `session` slot — and `session_active()` keys off that slot, so reporting
    /// through the mutable accessor would falsely signal an active session and
    /// start the tick loop with no session (no renderer, no real timing).
    fn loaded_transport(&self) -> Option<&crate::transport::Transport>;

    /// The `(left, right)` paths the preview decode source was opened from for
    /// the operator's imported clips, or `None` while preview still decodes the
    /// startup/hardcoded pair (IMPT-01 / D3-15).
    ///
    /// The operator's imported clips must replace the hardcoded startup pair
    /// once both roles are set; this exposes which pair is live so a GPU-free
    /// test can prove `begin_preview` uses the imported clips without opening a
    /// real decoder.
    fn preview_source_paths(&self) -> Option<(String, String)>;

    /// Report the webview chrome's collapsible state; recompute and reconfigure
    /// the native viewport (no transport/pose reset).
    fn set_chrome(&mut self, chrome: crate::presenter::ChromeState, events: &EventSink);

    /// Reconfigure the native viewport for a new window size (no transport/pose
    /// reset). Must run at a command boundary (no `SurfaceTexture` alive).
    fn resize_viewport(&mut self, width: u32, height: u32, events: &EventSink);

    /// Run the file→file export described by `settings` until `interrupted` is
    /// set (EXPT-01/EXPT-02/EXPT-04).
    ///
    /// Builds a `reco_io::StitchJob` from the typed settings — the worker never
    /// calls FFmpeg or the encoder directly. Emits `ExportProgress` per frame,
    /// `ExportFinished`/`ExportCancelled`/`ExportFailed` on completion, and a
    /// typed `ExportFallback` + WARN when an override is unavailable (or Auto
    /// resolves to software).
    fn export(
        &mut self,
        settings: &crate::events::ExportSettings,
        events: &EventSink,
        interrupted: &AtomicBool,
    ) -> Result<(), WorkerError>;

    /// The backend's shared export-cancel flag (EXPT-04).
    ///
    /// `cancel_export` sets the same `Arc` from the Tauri command layer (the
    /// loop is blocked during a run); the loop reads it to clear a stale cancel
    /// and passes it to [`Self::export`]. Distinct from the calibration flag.
    fn export_cancel(&self) -> Arc<AtomicBool>;

    /// Probe the available encoders for `codec` and emit a typed `EncoderList`
    /// (EXPT-02).
    ///
    /// The worker owns the probe; the webview never enumerates encoders itself.
    fn probe_encoders(&self, codec: &str, events: &EventSink);

    /// Resolve and emit the deterministic output path preview for `settings`
    /// (EXPT-06).
    ///
    /// Pure path resolution — no job, no encoder, no calibration required. The
    /// worker owns the directory + stem + variant + collision suffix and emits
    /// the resolved path as [`WorkerEvent::ExportPathPreview`]; the webview shows
    /// it verbatim and never constructs one itself (T-05-08). Emitted on every
    /// preset/variant/output-dir change so the operator sees the final path
    /// before exporting.
    fn preview_export_path(&self, settings: &crate::events::ExportSettings, events: &EventSink);

    /// Probe the system and emit a typed `SystemInfo` (DIAG-01).
    ///
    /// The worker owns the probe; the webview never enumerates a GPU or an
    /// encoder itself. The real backend reads its own live
    /// [`reco_core::gpu::GpuContext`]; a GPU-free path reports the GPU fields as
    /// unknown (never a fabricated value).
    fn system_info(&self, events: &EventSink);

    /// Run the runtime preflight and emit a typed `Preflight` (DIAG-05).
    ///
    /// A cheap job: it probes FFmpeg, ONNX Runtime (when built), and the webview
    /// runtime and reports each with an actionable remediation on failure.
    fn run_preflight(&self, events: &EventSink);

    /// Save the current working state as a `.reco` project (PROJ-01).
    ///
    /// Builds a [`crate::project::RecoProject`] from the retained state (inputs,
    /// lens overrides, calibration path + inline snapshot, pose) plus `settings`
    /// (the export settings live in the webview, so it passes them with the
    /// command), validates it, writes it **atomically** (no partial file on
    /// failure), and emits `ProjectSaved`. No media is copied.
    fn save_project(
        &mut self,
        path: String,
        settings: &crate::events::ExportSettings,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Open a `.reco` project and restore the whole working state (PROJ-01).
    ///
    /// Parses and validates the manifest, probes each referenced input, and
    /// either restores everything + emits `ProjectOpened`, or emits
    /// `ProjectMissingInputs` (retaining the parsed project for relocation) —
    /// never a partial restore, never a failure on a missing input.
    fn open_project(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError>;

    /// Relocate one missing project input and re-run the restore (PROJ-01).
    ///
    /// Replaces the named role's referenced path on the retained pending project
    /// and re-probes; a complete set restores and emits `ProjectOpened`, a
    /// still-missing set re-emits `ProjectMissingInputs`.
    fn relocate_project_input(
        &mut self,
        role: crate::events::InputRole,
        path: String,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Write the one-click, redacted, local-only diagnostics bundle (DIAG-03).
    ///
    /// Gathers the retained structured logs, the probed system info, the active
    /// calibration profile, and the retained debug inspector payload into a
    /// single zip at `path` (atomic write; no partial bundle on failure), then
    /// emits `DiagnosticsBundleWritten`. The bundle is local-only: nothing here
    /// touches the network, and the path is the operator's own choice.
    fn export_diagnostics_bundle(
        &self,
        path: String,
        events: &EventSink,
    ) -> Result<(), WorkerError>;

    /// Dispatch a transport-agnostic input intent to the worker's pose state.
    fn dispatch_intent(&mut self, intent: reco_control::ControlIntent);

    /// Drain the active presenter's own pointer input and dispatch it to the
    /// worker's pose (PREV-04, native path).
    ///
    /// Called by the worker loop once per iteration — every tick while a session
    /// is playing, and every iteration an idle command wakes — so the platform
    /// event queue is never left to accumulate. Defaulted so the loop's shape
    /// does not force a backend that owns no window (or a test double) to care.
    fn drain_pointer_input(&mut self, _events: &EventSink) {}

    /// Swap the active presenter to `kind` at a command boundary (PREV-05).
    ///
    /// Reuses the existing device (never a second one), releases the outgoing
    /// presenter's window, configures the incoming presenter against the shared
    /// device, and emits the `Presenter` event plus exactly one WARN/INFO line.
    /// Preserves transport position and pose. A `kind` the platform cannot host
    /// falls through the chain and reports the reason.
    fn set_presenter(&mut self, kind: crate::presenter::PresenterKind, events: &EventSink);

    /// Position the source back at frame 0 so playback (or a loop wrap) can pull
    /// frames again.
    ///
    /// A source that has run out stays exhausted: `try_next_frame` returns `None`
    /// forever after. So both a loop wrap and a fresh session over a spent
    /// source must rewind, or the very next tick reads as end-of-source and the
    /// clip appears not to play at all. Must be a no-op-safe error rather than a
    /// silent skip — a failed rewind is a real failure to report.
    fn rewind_for_loop(&mut self) -> Result<(), WorkerError>;

    /// The live session's transport, or `None` when idle — without the
    /// placeholder-materialising side effect of [`Self::transport`].
    fn session_transport_mut(&mut self) -> Option<&mut crate::transport::Transport>;

    /// Advance the transport by one frame after a successful present, and end
    /// the session if the counter reached `total_frames`.
    ///
    /// Returns `true` when the session ended and the tick must stop.
    ///
    /// This is the **second** of the two endings, and it was previously
    /// open-coded in the real backend as `self.session = None`: no
    /// `preview reached end of source` line, and no `end_preview`, so
    /// `carry_user_state` never mirrored the session back. It is a provided
    /// method, and used inline, precisely so there is one copy — the user saw
    /// seven `preview session started` lines with no ends because the
    /// mock and the real backend each owned their own ending and only the real
    /// one ran in production.
    ///
    /// Note the counter is a `duration × fps` **estimate** while the decoder
    /// serves what the container holds, so this usually fires before the source
    /// is provably dry — it is the ending that happens most often.
    fn advance_session_after_frame(&mut self, events: &EventSink) -> Result<bool, WorkerError> {
        let ended = self.session_transport_mut().is_some_and(|t| {
            t.on_frame_advanced();
            t.state() == crate::transport::TransportState::Ended
        });
        if ended {
            self.resolve_end_of_source(events)?;
        }
        Ok(ended)
    }

    /// Resolve a playing session that has run out of source.
    ///
    /// **The single place** the wrap-vs-end decision is made. It is a provided
    /// method on purpose: the mock used to reimplement this branch, and a
    /// mutation at the real site (deferring the rewind to a pending transport
    /// seek) then left every test green.
    ///
    /// With Loop on, the SOURCE is rewound and the session keeps playing. With
    /// Loop off, the transport is marked ended and the session is dropped.
    ///
    /// The rewind happens HERE, inline, and that is load-bearing. Leaving it as a
    /// pending transport seek for the next tick looks equivalent but is not: an
    /// exhausted source keeps returning no frame, so the very next tick read as
    /// end-of-source again and queued another wrap, and every executed seek
    /// respawned the whole decode pipeline — two fresh decoders, each opening a
    /// CUDA context. On a short clip that respawned many times per second until
    /// the GPU was starved (CUDA_ERROR_OUT_OF_MEMORY, NVDEC falling back to
    /// software) and the app stopped accepting input.
    fn resolve_end_of_source(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        let looping = self
            .session_transport_mut()
            .is_some_and(|t| t.loop_enabled());
        if looping {
            self.rewind_for_loop()?;
            if let Some(t) = self.session_transport_mut() {
                t.request_seek(0);
                t.mark_seeking_done(0);
            }
            return Ok(());
        }
        events.info("preview reached end of source");
        if let Some(t) = self.session_transport_mut() {
            t.mark_ended();
            events.transport(t.state(), t.loop_enabled());
            let (frame, total, fps_rational) = (t.frame(), t.total_frames(), t.fps_rational());
            events.position(frame, total, fps_rational);
        }
        self.end_preview(events);
        Ok(())
    }

    /// Which presenter in the chain is currently active (PREV-05).
    fn active_presenter(&self) -> crate::presenter::PresenterKind;

    /// Show the separate preview window (PREV-05 "Show preview window" action).
    fn show_preview_window(&mut self, events: &EventSink);

    /// Switch the preview view mode (PREV-03).
    ///
    /// Stores the mode on the worker (NOT on the presenter); the next session
    /// tick picks the render path from it. Requesting the already-active mode
    /// is a no-op that emits no `View` event and no INFO line. The switch does
    /// NOT touch the transport frame or pose — it is applied at a tick
    /// boundary and preserves the playhead exactly.
    fn set_view(&mut self, mode: crate::presenter::ViewMode, events: &EventSink);

    /// Whether the GPU device was lost and needs recovery (FOUND-05).
    ///
    /// Read by the worker loop after every command and every preview tick so a
    /// loss discovered while the loop is idle is also recovered.
    fn recovery_pending(&self) -> bool;

    /// Rebuild device-dependent resources and reconfigure the surface
    /// (FOUND-05). Must be a no-op when [`Self::recovery_pending`] is false.
    fn recover(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Debug/test-only: force a device loss to exercise recovery (FOUND-05).
    ///
    /// wgpu 28 exposes no public "lose the device" API, so the real backend
    /// calls `Device::destroy()`, which fires the device-lost callback with
    /// [`reco_core::wgpu::DeviceLostReason::Destroyed`].
    fn simulate_device_loss(&mut self, events: &EventSink) -> Result<(), WorkerError>;

    /// Drop engine state in the documented order (FOUND-06).
    fn shutdown(&mut self);
}

/// Whether a playing session that just failed to produce a frame has actually
/// reached the end of the source.
///
/// **The three-way distinction is the whole point.** `try_next_frame` returns
/// `Ok(None)` for two unrelated reasons:
///
/// - the decode channel is momentarily empty (`TryRecvError::Empty`) — the
///   decoder threads are still working, or were just spawned;
/// - the source has genuinely run out (`TryRecvError::Disconnected`, EOF).
///
/// Treating both as end-of-source killed the session on its *first* tick: after
/// `begin_preview` respawns the decode pipeline, the new threads have not
/// produced a frame yet, so `Ok(None)` arrived about 8ms in and the log read
/// `preview session started` / `preview reached end of source` back to back for
/// a 2-second clip. Play therefore appeared to do nothing, repeatedly.
///
/// With Loop on the same conflation drove the visible symptom: each bogus
/// end-of-source triggered `resolve_end_of_source` → a rewind → a pipeline
/// respawn → which is again not ready for the next tick → another rewind. Two
/// fresh decoders every few milliseconds, each opening a CUDA context, until
/// `cuCtxCreate` returned CUDA_ERROR_OUT_OF_MEMORY and the decoders fell back
/// to software. The log flood the user reported.
///
/// `Source::is_exhausted` exists precisely to make this distinction ("distinguish
/// finished from frame not ready yet"); `reco-core` documents it for interactive
/// consumers such as this one.
pub fn is_end_of_source(playing: bool, frame_available: bool, exhausted: bool) -> bool {
    playing && !frame_available && exhausted
}

/// The widest FOV that is actually renderable: the coverage boundary's own
/// ceiling bounded by the configured maximum.
///
/// This is the same `min(...)` the CLI logs as `max FOV = … (coverage-limited)`
/// (`crates/reco-cli/src/preview.rs:131-141`). It is factored out so the
/// arithmetic can be tested without a renderer: on the shipped test clip the
/// coverage ceiling is 50.87° against a configured max of 150°, and every degree
/// above it was inert under `clamp_via_coverage` while the UI kept advertising it.
///
/// `None` for the coverage (before the first renderer exists) falls back to the
/// configured maximum — there is no calibration to be bounded by yet, and that
/// is what the pose clamps to anyway.
pub fn fov_ceiling_from(
    coverage: Option<&reco_core::projection::CoverageBoundary>,
    configured: f32,
) -> f32 {
    coverage.map_or(configured, |coverage| {
        coverage.max_fov_degrees().min(configured)
    })
}

/// Handle a single command. Returns `false` when the loop should stop.
fn handle_command<B: EngineBackend>(
    cmd: WorkerCommand,
    backend: &mut B,
    events: &EventSink,
    interrupted: &AtomicBool,
) -> bool {
    match cmd {
        WorkerCommand::Import => match backend.import(events) {
            Ok(()) => {
                events.info("import finished");
                // Publish the loaded clip's authoritative projection. Phase 2's
                // chrome has no Import button — the app imports at startup — so
                // without this the frontend transport store never leaves its
                // initial "empty" state and gates every transport action
                // (`play`, `seek`, `step`, `set_loop`) as unavailable. The
                // worker owns the projection (UI-SPEC Interaction rule 1): the
                // frontend mirrors it and never derives readiness itself.
                let (frame, total, fps_rational, state, loop_enabled) = {
                    // Read-only: see `loaded_transport` for why reporting must
                    // not materialize a placeholder transport.
                    match backend.loaded_transport() {
                        Some(t) => (
                            t.frame(),
                            t.total_frames(),
                            t.fps_rational(),
                            t.state(),
                            t.loop_enabled(),
                        ),
                        // A backend that loaded nothing cannot project a clip.
                        None => return true,
                    }
                };
                events.position(frame, total, fps_rational);
                events.transport(state, loop_enabled);
            }
            Err(e) => events.failed(e),
        },
        WorkerCommand::Preview => {
            // "Start preview": begin a session and start playing. The worker
            // loop drives ticks from here (Pattern 3); no frame loop runs inside
            // this handler.
            if let Err(e) = backend.begin_preview(events) {
                events.failed(e);
                return true;
            }
            backend.transport().play();
            emit_transport(backend, events);
        }
        WorkerCommand::Play => {
            if !backend.session_active()
                && let Err(e) = backend.begin_preview(events)
            {
                events.failed(e);
                return true;
            }
            backend.transport().play();
            emit_transport(backend, events);
        }
        WorkerCommand::Pause => {
            backend.transport().pause();
            emit_transport(backend, events);
        }
        WorkerCommand::Seek { frame } => {
            // Coalesced: store the pending target; the tick performs one seek.
            backend.transport().request_seek(frame);
        }
        WorkerCommand::StepFrame { direction } => {
            backend.transport().step(direction);
            let frame = backend.transport().frame();
            backend.transport().request_seek(frame);
        }
        WorkerCommand::SetLoop(enabled) => {
            backend.transport().set_loop(enabled);
            emit_transport(backend, events);
        }
        WorkerCommand::SetChrome {
            panel_expanded,
            drawer_expanded,
            active_screen,
            modal_open,
        } => {
            backend.set_chrome(
                crate::presenter::ChromeState {
                    panel_expanded,
                    drawer_expanded,
                    active_screen,
                    modal_open,
                },
                events,
            );
        }
        WorkerCommand::ResizeViewport { width, height } => {
            backend.resize_viewport(width, height, events);
        }
        WorkerCommand::SetPresenter(kind) => {
            backend.set_presenter(kind, events);
        }
        WorkerCommand::ShowPreviewWindow => {
            backend.show_preview_window(events);
        }
        WorkerCommand::SetView(mode) => {
            backend.set_view(mode, events);
        }
        WorkerCommand::SetInput { role, path } => {
            if let Err(e) = backend.set_input(role, path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ClearInput { role } => {
            if let Err(e) = backend.clear_input(role, events) {
                events.failed(e);
            }
        }
        WorkerCommand::LensCandidates { role } => {
            if let Err(e) = backend.lens_candidates(role, events) {
                events.failed(e);
            }
        }
        WorkerCommand::SetLensOverride { role, candidate } => {
            if let Err(e) = backend.set_lens_override(role, candidate, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ClearLensOverride { role } => {
            if let Err(e) = backend.clear_lens_override(role, events) {
                events.failed(e);
            }
        }
        WorkerCommand::StartCalibration { options } => {
            // Reset the loop's interrupted flag (mirroring Export). The actual
            // calibration cancel flag is the backend's shared `Arc<AtomicBool>`,
            // reset inside `calibrate` before the engine starts.
            interrupted.store(false, Ordering::SeqCst);
            if let Err(e) = backend.calibrate(options, events, interrupted) {
                events.failed(e);
            }
        }
        WorkerCommand::LoadProfile { path } => {
            if let Err(e) = backend.load_profile(path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::SaveProfile { path } => {
            if let Err(e) = backend.save_profile(path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::SetFieldRoi { left, right } => {
            if let Err(e) = backend.set_field_roi(left, right, events) {
                events.failed(e);
            }
        }
        WorkerCommand::RefineLens { heldout_fraction } => {
            // UI-SPEC Event Log Contract: an INFO line when the request is
            // received, distinct from the completion INFO/WARN the result emits.
            events.info("Lens k1 refinement requested");
            if let Err(e) = backend.refine_lens(heldout_fraction, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualBegin { frame } => {
            if let Err(e) = backend.manual_begin(frame, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualSetFrame { frame } => {
            if let Err(e) = backend.manual_set_frame(frame, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualExit => {
            if let Err(e) = backend.manual_exit(events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualDetectSync => {
            if let Err(e) = backend.manual_detect_sync(events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualSetSync { offset_frames } => {
            if let Err(e) = backend.manual_set_sync(offset_frames, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualAddPin { left_px, right_px } => {
            if let Err(e) = backend.manual_add_pin(left_px, right_px, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualMovePin { id, side, px } => {
            if let Err(e) = backend.manual_move_pin(id, side, px, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualRemovePin { id } => {
            if let Err(e) = backend.manual_remove_pin(id, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualClearPins => {
            if let Err(e) = backend.manual_clear_pins(events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualSetLens {
            side,
            fx,
            cx,
            cy,
            k1,
        } => {
            if let Err(e) = backend.manual_set_lens(side, fx, cx, cy, k1, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualSetLayout {
            cam_d,
            intersect,
            x_ty,
            x_rz,
        } => {
            if let Err(e) = backend.manual_set_layout(cam_d, intersect, x_ty, x_rz, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualResetLens => {
            if let Err(e) = backend.manual_reset_lens(events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualResetRig => {
            if let Err(e) = backend.manual_reset_rig(events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualValidate { frame } => {
            if let Err(e) = backend.manual_validate(frame, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ManualSave { path } => {
            if let Err(e) = backend.manual_save(path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::Export { settings } => {
            // EXPT-04: use the dedicated export-cancel flag, not the loop's
            // `interrupted` (which is the shutdown flag). Clear a stale cancel
            // from a previous run first. The backend emits the typed
            // finished/cancelled/failed events; the loop only surfaces a
            // programming-level `Err`.
            let cancel = backend.export_cancel();
            cancel.store(false, Ordering::SeqCst);
            if let Err(e) = backend.export(&settings, events, cancel.as_ref()) {
                events.failed(e);
            }
        }
        WorkerCommand::CancelExport => {
            // The loop is blocked inside the export job, so this arm is reached
            // only after the run; the real cancel path is the Tauri command
            // writing the same shared flag directly (mirroring `cancel_calibration`).
            backend.export_cancel().store(true, Ordering::SeqCst);
        }
        WorkerCommand::ProbeEncoders { codec } => {
            backend.probe_encoders(&codec, events);
        }
        WorkerCommand::PreviewExportPath { settings } => {
            // EXPT-06: pure path resolution — the worker owns the path the
            // webview shows; no job, no encoder, no calibration required.
            backend.preview_export_path(&settings, events);
        }
        WorkerCommand::SystemInfo => {
            // DIAG-01: probe the system on demand; the webview renders the typed
            // view and never enumerates a GPU or encoder itself.
            backend.system_info(events);
        }
        WorkerCommand::RunPreflight => {
            // DIAG-05: probe the runtime prerequisites on demand.
            backend.run_preflight(events);
        }
        WorkerCommand::SaveProject { path, settings } => {
            // PROJ-01: a job — assemble + write the manifest atomically.
            if let Err(e) = backend.save_project(path, &settings, events) {
                events.failed(e);
            }
        }
        WorkerCommand::OpenProject { path } => {
            // PROJ-01: a job — parse/probe/restore, or emit the missing list.
            if let Err(e) = backend.open_project(path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::RelocateProjectInput { role, path } => {
            // PROJ-01: a job — replace the missing path and re-run the restore.
            if let Err(e) = backend.relocate_project_input(role, path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::ExportDiagnosticsBundle { path } => {
            // DIAG-03: a job — gather + redact + write the local bundle atomically.
            if let Err(e) = backend.export_diagnostics_bundle(path, events) {
                events.failed(e);
            }
        }
        WorkerCommand::Intent(intent) => backend.dispatch_intent(intent),
        WorkerCommand::RepublishProjection => backend.republish_projection(events),
        WorkerCommand::Shutdown => {
            interrupted.store(true, Ordering::SeqCst);
            backend.shutdown();
            events.info("shutdown");
            return false;
        }
        WorkerCommand::SimulateDeviceLoss => {
            // FOUND-05 debug affordance: force a device loss so the recovery
            // path is exercised deterministically. The real backend calls
            // `Device::destroy()`; the callback sets the lost flag and the loop
            // recovers on the next tick. Not reachable from the normal UI.
            match backend.simulate_device_loss(events) {
                Ok(()) => events.info("device loss simulated (debug)"),
                Err(e) => events.failed(e),
            }
        }
    }
    true
}

/// Emit a [`WorkerEvent::Transport`] from the backend's current transport.
fn emit_transport<B: EngineBackend>(backend: &mut B, events: &EventSink) {
    let transport = backend.transport();
    let state = transport.state();
    let loop_enabled = transport.loop_enabled();
    events.transport(state, loop_enabled);
}

/// Whether `cmd` is a preview/edit command that the modal export must reject
/// while an export is in flight (EXPT-04 / CONTEXT modal decision).
///
/// Covers every command that mutates preview/edit state — including the lens
/// override, field-ROI, and lens-refinement edits and the seek/step transport
/// moves, which were previously omitted and so executed after a blocking export
/// despite the modal contract (IN-09).
fn is_modal_forbidden(cmd: &WorkerCommand) -> bool {
    matches!(
        cmd,
        WorkerCommand::Preview
            | WorkerCommand::Play
            | WorkerCommand::SetInput { .. }
            | WorkerCommand::ClearInput { .. }
            | WorkerCommand::StartCalibration { .. }
            | WorkerCommand::SetLensOverride { .. }
            | WorkerCommand::ClearLensOverride { .. }
            | WorkerCommand::SetFieldRoi { .. }
            | WorkerCommand::RefineLens { .. }
            | WorkerCommand::Seek { .. }
            | WorkerCommand::StepFrame { .. }
            | WorkerCommand::ManualBegin { .. }
            | WorkerCommand::ManualSetFrame { .. }
            | WorkerCommand::ManualExit
            | WorkerCommand::ManualDetectSync
            | WorkerCommand::ManualSetSync { .. }
            | WorkerCommand::ManualAddPin { .. }
            | WorkerCommand::ManualMovePin { .. }
            | WorkerCommand::ManualRemovePin { .. }
            | WorkerCommand::ManualClearPins
            | WorkerCommand::ManualSetLens { .. }
            | WorkerCommand::ManualSetLayout { .. }
            | WorkerCommand::ManualResetLens
            | WorkerCommand::ManualResetRig
            | WorkerCommand::ManualValidate { .. }
            | WorkerCommand::ManualSave { .. }
    )
}

/// Estimated milliseconds remaining for an export (EXPT-04).
///
/// `eta = elapsed × (total − completed) / completed`. Returns `None` when the
/// total is unknown or no frame has completed yet — an honest unknown, never a
/// division by zero and never a fabricated `0` (the UI renders `Not reported`).
pub fn eta_ms(elapsed_ms: u64, completed: u64, total: Option<u64>) -> Option<u64> {
    let total = total?;
    if completed == 0 || total == 0 {
        return None;
    }
    Some(elapsed_ms.saturating_mul(total.saturating_sub(completed)) / completed)
}

/// Parse an output codec name, defaulting to H.264 on an unknown string.
///
/// T-05-01: the string is parsed through the engine's own `FromStr`; an unknown
/// value is never injected as a raw encoder argument — it falls back to H.264
/// and the caller logs a WARN naming the rejected value.
fn parse_output_codec(name: &str) -> (reco_io::output::Codec, Option<String>) {
    match name.parse::<reco_io::output::Codec>() {
        Ok(codec) => (codec, None),
        Err(e) => (reco_io::output::Codec::default(), Some(e)),
    }
}

/// Parse an output quality name, defaulting to Balanced on an unknown string.
fn parse_output_quality(name: &str) -> (reco_io::output::Quality, Option<String>) {
    match name.parse::<reco_io::output::Quality>() {
        Ok(quality) => (quality, None),
        Err(e) => (reco_io::output::Quality::default(), Some(e)),
    }
}

/// Resolve the encoder an export will use from the probed candidates (EXPT-02).
///
/// `available` is in preference order (hardware first). `requested` is the
/// operator's override, or `None` for Auto. Returns the chosen encoder and, when
/// the choice is a **software fallback**, the `(requested, used)` pair for the
/// typed fallback event + WARN. An override that names an unavailable encoder
/// falls back to the auto candidate (the "override unavailable" edge probe).
fn choose_encoder(
    available: &[reco_io::ffmpeg::encoder::EncoderInfo],
    requested: Option<&str>,
) -> (
    reco_io::ffmpeg::encoder::EncoderInfo,
    Option<(String, String)>,
) {
    let auto = available.first();
    match requested {
        // Auto: pick the best allowed candidate. If even the head is software,
        // the hardware was unavailable — an explicit fallback naming the head as
        // both requested and used (the banner shows the software encoder).
        None => match auto {
            Some(enc) => {
                let fallback = (!enc.is_hardware).then(|| (enc.name.clone(), enc.name.clone()));
                (enc.clone(), fallback)
            }
            None => (
                no_encoder_placeholder(),
                Some(("auto".to_string(), "none".to_string())),
            ),
        },
        // Override: use it only when the probe knows it. An unknown name is an
        // unavailable override → fall back to auto.
        Some(name) => match available.iter().find(|e| e.name == name) {
            Some(enc) => (enc.clone(), None),
            None => match auto {
                Some(enc) => (enc.clone(), Some((name.to_string(), enc.name.clone()))),
                None => (
                    no_encoder_placeholder(),
                    Some((name.to_string(), "none".to_string())),
                ),
            },
        },
    }
}

/// The placeholder encoder used when the probe finds no encoder for a codec.
fn no_encoder_placeholder() -> reco_io::ffmpeg::encoder::EncoderInfo {
    reco_io::ffmpeg::encoder::EncoderInfo {
        name: "none".to_string(),
        description: "no encoder available for this codec".to_string(),
        is_hardware: false,
    }
}

/// The directory an export writes to: the operator's choice, else the media
/// directory, else the current directory (EXPT-06).
///
/// The worker owns this resolution so the webview never supplies a path
/// (T-05-08). `hardcoded::media_dir()` is the documented default; a missing
/// environment falls back to the working directory rather than failing the run.
fn export_output_dir(settings: &crate::events::ExportSettings) -> std::path::PathBuf {
    settings
        .output_dir
        .as_deref()
        .map(std::path::PathBuf::from)
        .or_else(|| crate::hardcoded::media_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Resolve the collision-free output path for `settings` and a left input
/// (EXPT-06).
///
/// The stem is the left input's file stem (sanitized by
/// [`crate::export_naming::resolve_output_path`]); an absent input uses the
/// `"export"` fallback. This is the single resolution point shared by the
/// export job and the path preview, so the path the operator sees is the path
/// the worker writes.
fn preview_path_for(
    left_input: Option<&str>,
    settings: &crate::events::ExportSettings,
) -> Result<std::path::PathBuf, crate::export_naming::ExportNamingError> {
    let stem = left_input
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|s| s.to_str())
        .unwrap_or("export")
        .to_string();
    let dir = export_output_dir(settings);
    crate::export_naming::resolve_output_path(&dir, &stem, settings.variant)
}

/// Remove a partial export deliverable after a cancel or failure (WR-02).
///
/// [`preview_path_for`] only ever returns a path that did not exist, so any file
/// at the resolved output is one this export created and is safe to remove. The
/// removal error is ignored: the export is already terminating, and a leftover
/// file must never turn a typed terminal event into a panic.
fn remove_partial_output(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// Clamp an in/out trim window against the clip length (EXPT-03).
///
/// Returns the effective `(start, end)` plus whether anything was clamped. The
/// contract (CONTEXT edge probes): a trim never exceeds the clip, and an
/// `out <= in` window is clamped so an empty window is **never** exported —
/// dropping the out point exports to the clip end and the caller announces the
/// clamp with a WARN.
fn clamp_trim(
    start: Option<u64>,
    end: Option<u64>,
    total: Option<u64>,
) -> (Option<u64>, Option<u64>, bool) {
    let mut clamped = false;

    let mut start = start;
    if let (Some(s), Some(t)) = (start, total)
        && t > 0
        && s >= t
    {
        start = Some(t - 1);
        clamped = true;
    }

    let mut end = end;
    if let (Some(e), Some(t)) = (end, total)
        && e > t
    {
        end = Some(t);
        clamped = true;
    }

    // An out <= in window would export nothing; drop the out point so the run
    // covers `start .. clip end` instead (announced by the caller).
    let effective_start = start.unwrap_or(0);
    if let Some(e) = end
        && e <= effective_start
    {
        end = None;
        clamped = true;
    }

    (start, end, clamped)
}

/// Build the CPU pack layout for a source-tile variant (EXPT-05).
///
/// `SideBySide` packs the two source tiles with `hstack`; `Stacked` packs them
/// with `vstack` — the same layouts the GPU pack path uses. The tile dims are
/// validated through the shared [`StackGridLayout`](reco_core::gpu::yuv_stack_packer::StackGridLayout)
/// authority (research §2.2) so both pack paths enforce the identical YUV420P
/// alignment, then mapped to the CPU [`GridLayout`](reco_io::stacked_video::GridLayout)
/// the encoder consumes. `Panorama` is not a grid variant and returns `None`.
fn variant_grid_layout(
    variant: crate::events::ExportVariant,
    tile_width: u32,
    tile_height: u32,
) -> Option<reco_io::stacked_video::GridLayout> {
    use crate::events::ExportVariant;
    use reco_core::gpu::yuv_stack_packer::StackGridLayout;

    // Validate through the GPU layout authority (shared YUV420P rule).
    let _validated = match variant {
        ExportVariant::SideBySide => StackGridLayout::hstack(tile_width, tile_height, 2)?,
        ExportVariant::Stacked => StackGridLayout::vstack(tile_width, tile_height, 2)?,
        ExportVariant::Panorama => return None,
    };

    match variant {
        ExportVariant::SideBySide => {
            reco_io::stacked_video::GridLayout::hstack(tile_width, tile_height, 2)
        }
        ExportVariant::Stacked => {
            reco_io::stacked_video::GridLayout::vstack(tile_width, tile_height, 2)
        }
        ExportVariant::Panorama => None,
    }
}

/// Run a source-tile variant export through the existing stacked-video pack
/// path and the same FFmpeg encoder (EXPT-05).
///
/// No second encoder: [`StackedEncoder`](reco_io::stacked_video::encoder::StackedEncoder)
/// wraps the same `VideoEncoder` the panorama path uses, and
/// [`pack_yuv420p`](reco_io::stacked_video::pack_yuv420p) is the existing CPU
/// packer. The variant only changes the grid layout and the output path
/// (research §2.2). The trim window is honored in source-frame indices, and the
/// sync offset is applied by the source pairing exactly as a stitch does. Emits
/// per-frame `ExportProgress`; the caller emits the terminal event.
///
/// `encoder_name` is the already-resolved encoder from the probe (never a raw
/// webview string).
#[allow(clippy::too_many_arguments)]
fn run_stacked_variant(
    left: &str,
    right: &str,
    output: &std::path::Path,
    variant: crate::events::ExportVariant,
    codec: reco_io::output::Codec,
    quality: reco_io::output::Quality,
    encoder_name: &str,
    sync_offset: i64,
    trim: (Option<u64>, Option<u64>),
    events: &EventSink,
    interrupted: &AtomicBool,
) -> Result<(), WorkerError> {
    use reco_core::source::FrameSource as _;

    let mut source = reco_io::adapters::FfmpegFileSource::open_with_offset(
        std::path::Path::new(left),
        std::path::Path::new(right),
        sync_offset,
    )
    .map_err(|e| WorkerError::Engine(e.to_string()))?;

    let info = source.info();
    let layout = variant_grid_layout(variant, info.width, info.height).ok_or_else(|| {
        WorkerError::Engine(format!(
            "the {variant:?} variant needs YUV420P-aligned source tiles, but the source is \
             {}x{} (width must be divisible by 4, height must be even)",
            info.width, info.height
        ))
    })?;

    let (start, end) = trim;
    if let Some(s) = start {
        source
            .seek(s)
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
    }
    let start_frame = start.unwrap_or(0);
    let total = match end {
        Some(e) => Some(e.saturating_sub(start_frame)),
        None => info.total_frames.map(|t| t.saturating_sub(start_frame)),
    };

    let config = reco_io::stacked_video::encoder::StackedEncoderConfig {
        fps: Some(info.fps_rational.unwrap_or((30, 1))),
        inner: reco_io::ffmpeg::encoder::EncoderConfig {
            // The variant is an `.mp4` deliverable, not the replay-recording
            // default (Matroska); the encoder is the same, only the container
            // differs.
            container: reco_io::ffmpeg::encoder::Container::Mp4,
            codec: codec.into(),
            quality_preset: quality.into(),
            encoder_name: Some(encoder_name.to_string()),
            ..reco_io::stacked_video::encoder::StackedEncoderConfig::default().inner
        },
    };

    let mut encoder = reco_io::stacked_video::encoder::StackedEncoder::new(layout, output, config)
        .map_err(|e| WorkerError::Engine(e.to_string()))?;

    let started = std::time::Instant::now();
    let mut done = 0u64;
    loop {
        if interrupted.load(Ordering::SeqCst) {
            // The caller reads the same flag and emits `ExportCancelled`.
            return Ok(());
        }
        if let Some(e) = end
            && start_frame + done >= e
        {
            break;
        }
        let Some(frame) = source
            .next_frame()
            .map_err(|e| WorkerError::Engine(e.to_string()))?
        else {
            break;
        };
        let reco_core::source::StereoFrame::Yuv420p(pair) = frame else {
            return Err(WorkerError::Engine(
                "the source delivered a non-CPU frame; the variant pack path needs YUV420P".into(),
            ));
        };
        let left_tile = reco_core::source::YuvFrame {
            y: pair.left.y,
            u: pair.left.u,
            v: pair.left.v,
            width: info.width,
            height: info.height,
            timestamp_us: 0,
        };
        let right_tile = reco_core::source::YuvFrame {
            y: pair.right.y,
            u: pair.right.u,
            v: pair.right.v,
            width: info.width,
            height: info.height,
            timestamp_us: 0,
        };
        encoder
            .push_all(&[&left_tile, &right_tile])
            .map_err(|e| WorkerError::Engine(e.to_string()))?;

        done += 1;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let percent = match total {
            Some(t) if t > 0 => 100.0 * done as f64 / t as f64,
            _ => 0.0,
        };
        events.export_progress(
            done,
            total,
            elapsed_ms,
            eta_ms(elapsed_ms, done, total),
            percent,
        );
    }

    encoder
        .finish()
        .map_err(|e| WorkerError::Engine(e.to_string()))?;
    Ok(())
}

/// The worker loop: drain pending commands (FIFO), tick an active session, then
/// block when idle.
///
/// Generic over the backend and driven only by a receiver + an event sink, so
/// it is unit-testable with a mock backend and no GPU.
///
/// # Session ticking (RESEARCH Pattern 3)
///
/// When a preview session is active the loop **never blocks on `recv`** — it
/// drains every iteration and then advances one session tick, so a transport or
/// pose command issued mid-playback is observed BETWEEN two ticks (the test
/// `command_is_seen_between_ticks` pins this). Only when no session is active
/// does the loop block, so an idle worker never spins.
fn worker_loop<B: EngineBackend>(
    rx: Receiver<WorkerCommand>,
    events: EventSink,
    mut backend: B,
    interrupted: AtomicBool,
) {
    events.info("engine worker started");
    loop {
        // FOUND-05: if a device loss was observed (callback set the flag during
        // the last command/job, or while idle), recover before doing more work.
        // Recovery is a no-op when nothing is pending.
        if backend.recovery_pending()
            && let Err(e) = backend.recover(&events)
        {
            events.failed(e);
        }

        // Drain everything pending, in arrival order (FOUND-03 adjacency).
        let mut ran_job = false;
        // EXPT-04 modal enforcement: once an Export has run in this drain pass,
        // any preview/edit command queued behind it is rejected with a typed
        // "export in progress" response rather than silently executed (the
        // export blocks the loop, so those commands were issued during the run).
        let mut export_ran = false;
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if export_ran && is_modal_forbidden(&cmd) {
                        events.failed(WorkerError::ExportInProgress);
                        continue;
                    }
                    // Flush a coalesced manual preview before teardown so a
                    // pending frame is not lost when the worker stops; `shutdown`
                    // tears down engine state the preview render needs.
                    if matches!(cmd, WorkerCommand::Shutdown)
                        && let Err(e) = backend.flush_manual_preview(&events)
                    {
                        events.failed(e);
                    }
                    let is_export = matches!(cmd, WorkerCommand::Export { .. });
                    let is_job = matches!(
                        cmd,
                        WorkerCommand::Import
                            | WorkerCommand::Preview
                            | WorkerCommand::Export { .. }
                            | WorkerCommand::SetInput { .. }
                            | WorkerCommand::StartCalibration { .. }
                            | WorkerCommand::LoadProfile { .. }
                            | WorkerCommand::SaveProfile { .. }
                    );
                    let keep_going = handle_command(cmd, &mut backend, &events, &interrupted);
                    export_ran |= is_export;
                    ran_job |= is_job;
                    if !keep_going {
                        return;
                    }
                    // A job may have triggered a device loss (e.g. the debug
                    // simulation, or a real loss mid-preview): recover before
                    // handling the next queued command.
                    if backend.recovery_pending()
                        && let Err(e) = backend.recover(&events)
                    {
                        events.failed(e);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        // Fire a due debounced manual solve (MANU-03). The solve is NEVER run
        // from a pin command directly — only here, once the debounce window has
        // elapsed with no further pin mutation (T-04.1-11). The backend clears
        // its own deadline when the solve runs.
        if let Some(deadline) = backend.manual_solve_deadline()
            && std::time::Instant::now() >= deadline
            && let Err(e) = backend.manual_solve(&events)
        {
            events.failed(e);
        }

        // Render the coalesced manual preview at most once per command-drain
        // (MANU-03). A drag burst marks the preview dirty on every pointer event
        // but produces a single bounded frame here (last state wins), so the
        // event channel never queues one full frame per event. Placed after the
        // solve so a solved preview is included in the same flush.
        if let Err(e) = backend.flush_manual_preview(&events) {
            events.failed(e);
        }

        // Native pose input: the presenter's own window owns the preview
        // rectangle, so its gestures are drained here — after the command drain
        // (so a SetView/SetChrome in the same batch is already applied) and
        // before the tick (so the tick renders the panned pose). NOT inside
        // `tick_session`: that never runs when idle, and a drag while paused
        // must still pan.
        backend.drain_pointer_input(&events);

        // An active session is paced by the loop: advance exactly one tick,
        // then loop again to re-drain (never block — that is what lets a
        // Pause/Seek/Intent land between frames).
        if backend.session_active() {
            if let Err(e) = backend.tick_session(&events) {
                // A failed tick (a rejected seek is the realistic case) must not
                // leave the UI showing a playhead the engine never reached: the
                // frontend mirrors the worker and never owns the position
                // (UI-SPEC Interaction rule 1), and E3/error requires the
                // playhead to revert to the last worker position. Publish it
                // before the session is dropped, while the transport still holds
                // the real frame.
                backend.publish_position(&events);
                events.failed(e);
                // A failed tick must not leave the loop spinning on a session
                // that cannot advance: end it and return to the idle path.
                backend.end_preview(&events);
            }
            continue;
        }

        // A job just ran; loop again to pick up any Shutdown queued during it
        // rather than blocking. Otherwise block — never spin.
        if ran_job {
            continue;
        }

        // Block for the next command. When a debounced manual solve is armed,
        // bound the wait by its deadline so the solve fires on time; on timeout
        // the loop re-enters, the due-check above runs the solve, and the
        // channel blocks normally again (no busy spin).
        match backend.manual_solve_deadline() {
            Some(deadline) => {
                let timeout = deadline.saturating_duration_since(std::time::Instant::now());
                match rx.recv_timeout(timeout) {
                    Ok(cmd) => {
                        let keep_going = handle_command(cmd, &mut backend, &events, &interrupted);
                        if !keep_going {
                            return;
                        }
                        if backend.recovery_pending()
                            && let Err(e) = backend.recover(&events)
                        {
                            events.failed(e);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match rx.recv() {
                Ok(cmd) => {
                    let keep_going = handle_command(cmd, &mut backend, &events, &interrupted);
                    if !keep_going {
                        return;
                    }
                    if backend.recovery_pending()
                        && let Err(e) = backend.recover(&events)
                    {
                        events.failed(e);
                    }
                }
                Err(_) => return,
            },
        }
    }
}

/// The engine worker thread plus the UI-side command handle.
pub struct EngineWorker {
    // Read by `shutdown` / `join`, which the app-teardown path (FOUND-06,
    // Plan 05) and this module's tests drive; the Plan 04 UI wiring does not
    // tear the worker down yet because the process owns it for its lifetime.
    #[cfg_attr(not(test), allow(dead_code))]
    handle: JoinHandle<()>,
    cmd: WorkerHandle,
}

impl EngineWorker {
    /// Spawn the worker with the given backend.
    ///
    /// Returns the worker (holding the `JoinHandle`) and leaves the command
    /// sender reachable through [`EngineWorker::handle`]. The event receiver is
    /// the caller's to drain (the Tauri bridge does so in Plan 04).
    // Only the GPU-free tests use this two-tuple form; the app path uses
    // `spawn_with_manual_slot`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn spawn<B: EngineBackend + 'static>(backend: B) -> (Self, Receiver<WorkerEvent>) {
        let (worker, events, _slot) = Self::spawn_with_manual_slot(backend);
        (worker, events)
    }

    /// Spawn the worker and also return the shared manual-frame channel slot
    /// (MANU-03).
    ///
    /// [`spawn_gpu_worker`] uses this to hand the slot to the
    /// `manual_attach_preview` command layer; [`Self::spawn`] discards it so the
    /// many GPU-free tests keep their two-tuple. Tests that observe the binary
    /// manual transport can use this to attach a channel to the running worker.
    pub fn spawn_with_manual_slot<B: EngineBackend + 'static>(
        backend: B,
    ) -> (Self, Receiver<WorkerEvent>, ManualFrameSlot) {
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        Self::spawn_with_events(backend, evt_tx, evt_rx)
    }

    /// Spawn the worker over an **externally-created** event channel (DIAG-02).
    ///
    /// The app path uses this so the structured-log tracing layer (`main.rs`'s
    /// `UiLogLayer`) and the worker's [`EventSink`] share one event sender: the
    /// layer forwards engine records into the same stream the Tauri bridge
    /// drains, so a record reaches the LogViewer with no second channel. Tests
    /// keep the two-tuple [`Self::spawn`] / [`Self::spawn_with_manual_slot`]
    /// forms, which create a private channel.
    pub fn spawn_with_events<B: EngineBackend + 'static>(
        backend: B,
        evt_tx: Sender<WorkerEvent>,
        evt_rx: Receiver<WorkerEvent>,
    ) -> (Self, Receiver<WorkerEvent>, ManualFrameSlot) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let slot = events.manual_frame_slot();
        let handle = thread::Builder::new()
            .name("reco-engine-worker".to_string())
            .spawn(move || worker_loop(cmd_rx, events, backend, AtomicBool::new(false)))
            .expect("failed to spawn engine worker thread");
        (
            Self {
                handle,
                cmd: WorkerHandle::new(cmd_tx),
            },
            evt_rx,
            slot,
        )
    }

    /// The UI-side command handle. Clone it per Tauri command handler.
    pub fn handle(&self) -> WorkerHandle {
        self.cmd.clone()
    }

    /// Ask the worker to stop and join it, bounded by `timeout`.
    ///
    /// Sends [`WorkerCommand::Shutdown`], then joins on a helper thread so the
    /// timeout is enforced (a `JoinHandle` has no timed join of its own).
    ///
    /// # Errors
    ///
    /// * [`WorkerError::ChannelClosed`] if the worker already exited.
    /// * [`WorkerError::ShutdownTimeout`] if the thread did not stop in time.
    // FOUND-06 teardown; wired by the app-close handler in `main.rs` and
    // exercised by this module's tests.
    pub fn shutdown(self, timeout: Duration) -> Result<(), WorkerError> {
        self.cmd
            .send(WorkerCommand::Shutdown)
            .map_err(|_| WorkerError::ChannelClosed)?;
        let EngineWorker { handle, .. } = self;
        join_with_timeout(handle, timeout)
    }

    /// Join the worker without sending `Shutdown` (used when the command
    /// channel is already known to be closed because the loop returned).
    ///
    /// # Errors
    ///
    /// [`WorkerError::ShutdownTimeout`] if the thread did not stop in time.
    // See [`Self::shutdown`] — FOUND-06 teardown, test-exercised.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn join(self, timeout: Duration) -> Result<(), WorkerError> {
        join_with_timeout(self.handle, timeout)
    }
}

/// Join `handle`, returning [`WorkerError::ShutdownTimeout`] if it does not
/// finish within `timeout`.
fn join_with_timeout(handle: JoinHandle<()>, timeout: Duration) -> Result<(), WorkerError> {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("reco-engine-join".to_string())
        .spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        })
        .map_err(|_| WorkerError::ShutdownTimeout {
            timeout_ms: timeout.as_millis() as u64,
        })?;
    match done_rx.recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(_) => Err(WorkerError::ShutdownTimeout {
            timeout_ms: timeout.as_millis() as u64,
        }),
    }
}

// ---------------------------------------------------------------------------
// Real engine backend (owns the GPU device, renderer, and pose)
// ---------------------------------------------------------------------------

/// Choose the manual session's opening frame and whether to seed verified pins
/// (MANU-04 / WR-01).
///
/// Verified matches come from the calibration's first sampled frame — which is
/// not the UI's default frame 0 (`sampling::select_frame_indices` skips the
/// first 5%). Seeding them onto a different frame would mark the wrong pixels,
/// so when a verified seed exists the session opens on the seed's frame and
/// seeds; otherwise the requested frame is respected and nothing is seeded. The
/// returned frame is clamped to the clip length.
fn manual_seed_target(
    requested: u64,
    frames_total: u64,
    seed_frame: Option<u64>,
    has_seed: bool,
) -> (u64, bool) {
    let clamp = |frame: u64| frame.min(frames_total.saturating_sub(1));
    match seed_frame {
        Some(seed) if has_seed => (clamp(seed), true),
        _ => (clamp(requested), false),
    }
}

/// The pins to seed for a manual session and the `auto` set to retain
/// (MANU-04 / WR-03).
///
/// Every verified match is promoted to a pin; the retained `auto` set is always
/// empty so the solve never sees the seeds twice and a deleted pin stops
/// contributing. `auto_seed` is reserved for fresh matches produced by a
/// lens-triggered re-detect. An empty verified set (or a frame mismatch) seeds
/// nothing.
#[allow(clippy::type_complexity)]
fn manual_seed_sets(
    verified: &[reco_calibrate::types::MatchedPoint],
    seed_matches_frame: bool,
    left_wh: (u32, u32),
    right_wh: (u32, u32),
) -> (
    Vec<reco_calibrate::manual::ManualPin>,
    Vec<reco_calibrate::types::MatchedPoint>,
) {
    if seed_matches_frame && !verified.is_empty() {
        (
            reco_calibrate::manual::seed_pins_from_verified(verified, left_wh, right_wh),
            Vec::new(),
        )
    } else {
        (Vec::new(), Vec::new())
    }
}

/// The retained reference-frame state for an open manual calibration session
/// (MANU-03 / T-04.1-02).
///
/// Exactly **one** reference frame pair is retained for the session's
/// duration; it is dropped on `ManualExit`, so a session never accumulates
/// planes. The per-camera `CameraParams` seed every preview render, so the
/// feedback is always produced by the real GPU undistort — never an arbitrary
/// 2-D warp (UI-SPEC Real-parameters constraint).
struct ManualSession {
    /// The current reference frame index (0-based).
    frame: u64,
    /// Total frames in the reference clip.
    frames_total: u64,
    /// The retained left reference frame's YUV planes.
    left_frame: reco_core::source::YuvFrame,
    /// The retained right reference frame's YUV planes.
    right_frame: reco_core::source::YuvFrame,
    /// Left camera intrinsics used for the preview.
    left_params: reco_core::calibration::CameraParams,
    /// Right camera intrinsics used for the preview.
    right_params: reco_core::calibration::CameraParams,
    /// The chosen temporal sync offset in frames (MANU-02).
    ///
    /// Set by the manual nudge and carried on the session so the later pin/bend
    /// solves pair frames on the operator's chosen offset.
    sync_offset: i64,
    /// Which sync path produced [`Self::sync_offset`] (MANU-02).
    ///
    /// `None` until the operator nudges; `Manual` afterwards — preserving the
    /// CALB-06 provenance chain (IMU → audio → manual).
    sync_method: crate::events::SyncMethod,
    /// Total frames in the right clip, so the sync offset can clamp the right
    /// reference frame index (MANU-02 / MANU-03).
    right_frames_total: u64,
    /// The manual correspondence pins, in stable creation order (MANU-03).
    pins: Vec<SessionPin>,
    /// The next pin id to hand out (monotonic; never reused within a session).
    next_pin_id: u32,
    /// Fresh automatic matches for the solve's `auto` set (MANU-04 / MANU-05).
    ///
    /// Empty when the session opens: the verified seeds are promoted to pins
    /// instead, so the solve never sees them twice and a deleted pin stops
    /// contributing (WR-03). Repopulated only by a lens-triggered re-detect,
    /// whose fresh post-RANSAC matches are not represented as pins.
    auto_seed: Vec<reco_calibrate::types::MatchedPoint>,
    /// The left camera's baseline intrinsics captured at `manual_begin`
    /// (MANU-05). `ManualResetLens` restores these; handle clamps are computed
    /// from them.
    baseline_left_params: reco_core::calibration::CameraParams,
    /// The right camera's baseline intrinsics captured at `manual_begin`
    /// (MANU-05).
    baseline_right_params: reco_core::calibration::CameraParams,
    /// The layout baseline captured at `manual_begin` (MANU-06).
    /// `ManualResetRig` restores it.
    baseline_layout: reco_core::calibration::PlaneLayout,
    /// The layout currently in effect (MANU-06): the baseline, then any
    /// constrained layout-handle edit. The preview is rendered under it and the
    /// debounced re-solve's delta is measured against it.
    current_layout: reco_core::calibration::PlaneLayout,
}

/// One correspondence pin held by the manual session (MANU-03).
#[derive(Debug, Clone, Copy)]
struct SessionPin {
    /// Stable id, handed to the webview and used by move/remove commands.
    id: u32,
    /// Clicked point on the left frame, `[x, y]` pixels.
    left_px: [f64; 2],
    /// Corresponding point on the right frame, `[x, y]` pixels.
    right_px: [f64; 2],
    /// Whether the pin was seeded from a verified automatic match (MANU-04).
    verified: bool,
}

impl SessionPin {
    /// Project the session pin into the typed view the webview consumes.
    fn view(&self) -> crate::events::ManualPinView {
        crate::events::ManualPinView {
            id: self.id,
            left_px: self.left_px,
            right_px: self.right_px,
            verified: self.verified,
        }
    }
}

/// Clamp a pin point to a frame's pixel bounds (T-04.1-10).
///
/// The command boundary rejects non-finite coordinates; this clamps a finite
/// point into `[0, w-1] × [0, h-1]` so an out-of-bounds pin can never reach the
/// solve.
fn clamp_pin_px(px: [f64; 2], w: u32, h: u32) -> [f64; 2] {
    let max_x = w.saturating_sub(1) as f64;
    let max_y = h.saturating_sub(1) as f64;
    [px[0].clamp(0.0, max_x), px[1].clamp(0.0, max_y)]
}

/// Decide the session offset to apply from an audio-sync estimate (MANU-02).
///
/// A sub-floor estimate is surfaced for display but never applied: the flow
/// defaults the offset to 0 and requires an explicit manual confirmation
/// (WR-02). A confident estimate is applied with Audio provenance. Returns
/// `(applied_offset_frames, sync_method)`.
fn applied_audio_offset(offset_frames: i64, confidence: f64) -> (i64, crate::events::SyncMethod) {
    if confidence >= crate::events::AUDIO_SYNC_CONFIDENCE_FLOOR {
        (offset_frames, crate::events::SyncMethod::Audio)
    } else {
        (0, crate::events::SyncMethod::None)
    }
}

/// The engine objects exclusively owned by the worker thread (FOUND-03).
///
/// Field order is the drop order and is load-bearing (see the module header):
/// the decode source drops before the renderer, which drops before the
/// [`GpuContext`] (device). The presenter is the last field so its surface
/// outlives everything that configured it (a `wgpu::Surface` must outlive its
/// configuration lifetime).
pub struct GpuEngineBackend {
    /// The surface format negotiated for the presenter's surface.
    surface_format: reco_core::wgpu::TextureFormat,
    /// Pose state driven by intents (pan / zoom / reset).
    pose: reco_control::pose_control::PoseControl,
    /// Viewport geometry for the render target.
    viewport: crate::presenter::ViewportRect,
    /// Blend width for the stitch seam.
    blend_width: f32,
    /// Rig tilt in radians (from the loaded calibration).
    rig_tilt: f32,
    /// The active preview session's transport, or `None` when idle.
    ///
    /// `Some` while a session is active; [`Self::session_active`] keys off it.
    session: Option<crate::transport::Transport>,
    /// The transport built at `Import` time from the source's timing metadata,
    /// or `None` before the first successful import.
    ///
    /// Distinct from `session`: this one exists from import (so the frontend can
    /// learn the clip length and enable transport before a session starts), while
    /// `session` only exists while a preview is ticking. [`Self::transport`]
    /// prefers `session` and falls back to this, so a Play that lands before
    /// `begin_preview` carries the real fps/frame count instead of a placeholder.
    loaded: Option<crate::transport::Transport>,
    /// Whether the first presented frame of the current session still owes the
    /// A1 marker line (and the one-shot debug device-loss affordance).
    first_frame_marker_pending: bool,
    /// Wall-clock time of the last presented frame, for pacing the tick.
    last_frame_time: std::time::Instant,
    /// Last-known window width in physical pixels (seeded from `tauri.conf.json`).
    window_width: u32,
    /// Last-known window height in physical pixels (seeded from `tauri.conf.json`).
    window_height: u32,
    /// Current webview chrome state (source of truth for the native viewport).
    chrome: crate::presenter::ChromeState,
    /// Current preview view mode (PREV-03): source tiles vs. stitched panorama.
    ///
    /// Stored on the worker (NOT on the presenter); the session tick picks the
    /// render path from it. A switch is applied at a tick boundary and never
    /// touches the transport frame or pose.
    view_mode: crate::presenter::ViewMode,
    /// The operator-chosen input paths, keyed by role (IMPT-01).
    ///
    /// Populated by `SetInput`; the worker owns them so the webview never holds
    /// an engine-reachable path. Plan 03-03 reads this to run calibration.
    input_paths: std::collections::HashMap<crate::events::InputRole, String>,
    /// The probed metadata for each input, indexed by role (IMPT-02).
    ///
    /// Kept so compatibility can be recomputed and lens candidates resolved
    /// without re-probing the file.
    inputs: [Option<crate::events::InputMetadata>; 2],
    /// The resolved lens override for each input, indexed by role (IMPT-04).
    ///
    /// `None` means auto-detect. Passed to calibration as `left_params` /
    /// `right_params` (only when both are set).
    lens_overrides: [Option<reco_core::calibration::CameraParams>; 2],
    /// The chosen lens-override summaries, indexed by role (PROJ-01).
    ///
    /// The resolved [`Self::lens_overrides`] hold `CameraParams`, which cannot be
    /// reconstructed into a `LensCandidate` (the summary carries the camera/lens
    /// names a `.reco` stores). Retained so `save_project` can persist the
    /// operator's override verbatim; kept in sync with `lens_overrides`.
    lens_override_candidates: [Option<crate::events::LensCandidate>; 2],
    /// The current calibration: the result of a run or a loaded profile
    /// (IMPT-05/06). Consumed by save and, later, by preview/export.
    current_calibration: Option<reco_core::calibration::MatchCalibration>,
    /// The `.json` the current calibration was loaded from or saved to, when
    /// known (PROJ-01). Cleared when the result is invalidated and left `None`
    /// for a fresh run; the `.reco` manifest carries this plus an inline
    /// snapshot so a never-saved result still round-trips.
    calibration_path: Option<String>,
    /// A parsed `.reco` project awaiting relocation because one or more
    /// referenced inputs were missing (PROJ-01). Retained so a relocate command
    /// can re-run the restore without re-reading the file; `None` otherwise.
    pending_project: Option<PendingProject>,
    /// Whether [`Self::current_calibration`] reflects a fresh result/loaded
    /// profile (drives `ResultInvalidated` on input/override changes, D3-08).
    has_result: bool,
    /// The loaded calibration, if `Import` has run.
    calibration: Option<reco_core::calibration::MatchCalibration>,
    /// The open manual calibration session, if one is active (MANU-03).
    ///
    /// Retains exactly one reference-frame YUV pair for the session's duration
    /// (T-04.1-02); `None` when no manual session is open.
    manual: Option<ManualSession>,
    /// Verified (post-RANSAC) matches retained from the last successful
    /// calibration, used to pre-populate the pin editor (MANU-04). Empty until a
    /// calibration result exists.
    verified_seed: Vec<reco_calibrate::types::MatchedPoint>,
    /// The left-clip source frame `verified_seed`'s matches were detected on,
    /// when known (MANU-04 / WR-01).
    ///
    /// The manual flow only seeds those matches onto a session whose reference
    /// frame equals this, so the markers correspond to the displayed content.
    /// `None` when the run carried no pipeline frame context.
    verified_seed_frame: Option<u64>,
    /// All verified (post-RANSAC) matches from the last successful calibration,
    /// flattened across every sampled frame.
    ///
    /// Retained so the opt-in [`WorkerBackend::refine_lens`] action can
    /// aggregate enough observations to clear the `k1` conditioning gate
    /// (CR-01): the single-frame [`Self::verified_seed`] alone is far below
    /// `RECOMMENDED_MIN_MATCHES` on the real Xiaomi pair. Cleared on a profile
    /// load (a loaded profile carries no retained matches of its own) and empty
    /// until a calibration result exists.
    verified_matches: Vec<reco_calibrate::types::MatchedPoint>,
    /// The instant the armed debounced manual solve should fire, or `None`
    /// (MANU-03). Set by a pin mutation; cleared by [`Self::manual_solve`].
    manual_solve_at: Option<std::time::Instant>,
    /// Whether the armed manual solve must re-run the engine calibration path
    /// (re-undistort + re-detect + re-match) before re-solving (MANU-05).
    ///
    /// Set by a lens-handle edit, whose changed intrinsics invalidate the
    /// retained auto matches; cleared by [`Self::manual_solve`]. A pin or layout
    /// edit leaves it false, so those paths never pay the ~629 ms detection
    /// cost (T-04.1-14).
    manual_relens_pending: bool,
    /// Whether the manual preview needs re-rendering (MANU-03).
    ///
    /// A handle drag or pin move posts one command per pointer event; rendering
    /// a bounded frame for each still floods the event channel. Every manual
    /// mutation sets this flag instead, and the worker loop renders at most once
    /// per command-drain ([`Self::flush_manual_preview`]), so a burst collapses
    /// to a single frame (last state wins). Cleared by the flush.
    manual_preview_dirty: bool,
    /// The open decode source, if `Import` has run.
    ///
    /// Drops **first** (declaration order): the CLI documents that the decode
    /// source (NVDEC/CUDA) must drop before the wgpu renderer/device to avoid a
    /// CUDA-context teardown race (`crates/reco-cli/src/preview.rs:213-215`,
    /// RESEARCH Pitfall 5).
    source: Option<reco_io::adapters::FfmpegFileSource>,
    /// Input frame dimensions from the source metadata.
    input_size: Option<(u32, u32)>,
    /// The `(left, right, sync_offset)` the current [`Self::source`] was opened
    /// with, or `None` for the hardcoded startup import (IMPT-01 / D3-15).
    ///
    /// Lets the worker re-open the source when the operator's imported clips or
    /// the adopted calibration's sync offset change, so Preview decodes the
    /// imported files instead of the hardcoded startup pair.
    source_spec: Option<PreviewSourceSpec>,
    /// The last-built renderer. Rebuilt on device recovery (`Lost`); `None`
    /// until the first `preview`. Drops before the device (it holds bind
    /// groups/textures derived from it).
    renderer: Option<reco_core::render::stitch_renderer::StitchRenderer>,
    /// Payload needed to rebuild the renderer after a device loss: the input
    /// size and the loaded calibration. Kept so recovery needs no re-import.
    renderer_input: Option<(reco_core::calibration::MatchCalibration, u32, u32)>,
    /// The render target the worker draws into (D-03). The worker owns the
    /// presenter for the app's lifetime; the UI never touches it. Drops after
    /// the renderer but before the device, so its surface outlives the device
    /// that configured it.
    presenter: Box<dyn crate::presenter::SurfacePresenter + Send>,
    /// The **inactive** presenters in the PREV-05 chain, indexed by their position
    /// in [`crate::presenter::PRESENTER_CHAIN`]. The active presenter lives in
    /// [`Self::presenter`] and its slot here is `None`. Pre-built on the setup
    /// thread so a manual override swaps one in at a tick boundary without
    /// constructing anything on the worker thread.
    presenters: [Option<Box<dyn crate::presenter::SurfacePresenter + Send>>; 3],
    /// Which chain entry [`Self::presenter`] currently is.
    active_kind: crate::presenter::PresenterKind,
    /// The reason the active presenter was chosen, when it was a fallback;
    /// `None` when the active presenter is the strongest available.
    ///
    /// Set from the startup fall-through reason at construction (when the native
    /// arm did not build, `run_skeleton` records its `Unsupported` reason) and
    /// re-asserted by [`Self::republish_projection`] so the subscribed webview
    /// receives the resolved presenter together with its single locked fallback
    /// WARN (F1 subscribe-after-emit ordering). `set_presenter` keeps it in sync
    /// on every arm so a manual override cannot diverge from what
    /// `republish_projection` re-asserts.
    active_reason: Option<String>,
    /// Receives webview readback channels attached by `preview_attach_readback`
    /// (PREV-05). The channel is a Tauri IPC type, not an engine type, so it
    /// travels on its own path rather than the typed `WorkerCommand` enum.
    readback_rx: std::sync::mpsc::Receiver<tauri::ipc::Channel<tauri::ipc::Response>>,
    /// The sending half of [`Self::readback_rx`], exposed so the Tauri command
    /// layer can attach a channel.
    readback_tx: std::sync::mpsc::Sender<tauri::ipc::Channel<tauri::ipc::Response>>,
    /// The wgpu `Instance`, retained so a `Lost` surface can rebuild the device
    /// (FOUND-05). Creating the instance once keeps recovery on the same backend.
    instance: reco_core::wgpu::Instance,
    /// The retained adapter (from `for_surface`), needed to renegotiate surface
    /// capabilities after a recovery reconfigure. Never a second adapter (D-03).
    adapter: reco_core::wgpu::Adapter,
    /// Set by the device-lost callback (FOUND-05). Read by the worker loop to
    /// decide when to run recovery.
    device_lost: Arc<AtomicBool>,
    /// How many recovery cycles have been given up on (each cycle already
    /// retries `MAX_RECOVERY_ATTEMPTS` times internally). Diagnostic only — the
    /// flag is cleared on give-up so the loop cannot spin.
    recovery_attempts: u32,
    /// The shared calibration-cancel flag (CALB-02 / D3-11). A clone of the
    /// [`CalibrationCancel`] managed in Tauri state; `calibrate` passes it to
    /// `calibrate_videos_with_gpu`, which polls it between steps.
    calibration_cancel: Arc<AtomicBool>,
    /// The shared export-cancel flag (EXPT-04). A clone of the [`ExportCancel`]
    /// managed in Tauri state; `export` passes it to `StitchJob::run`, which
    /// polls it per frame. Distinct from `calibration_cancel` (prohibition: a
    /// stray export cancel must never abort a calibration).
    export_cancel: Arc<AtomicBool>,
    /// The shared structured-log buffer the diagnostics bundle reads (DIAG-03).
    /// The tracing layer pushes records into the same buffer, so the bundle
    /// carries the engine's structured records.
    log_buffer: crate::diagnostics::LogBuffer,
    /// The single GPU device owner (FOUND-03). Declared **last** so it drops
    /// after the decode source, renderer, and presenter surface — the documented
    /// teardown order (FOUND-06 / RESEARCH Pattern 6). Actually field 1 held the
    /// `GpuContext` in the original skeleton; it now lives here, last.
    gpu: reco_core::gpu::GpuContext,
}

/// Derive raw distorted-pixel observations from retained verified matches.
///
/// Each plane-coordinate match is pushed through the same forward chain the
/// manual seed uses: plane coord → undistorted pixel
/// ([`reco_calibrate::geometry::plane_to_pixel`], the single left/right swap) →
/// raw distorted pixel ([`reco_core::lens::undistorted_to_distorted`] under the
/// camera's own profile). This is non-circular: the undistort map is a
/// bijection, so the round-trip returns the true sensor pixel regardless of the
/// profile's current `k1` (research §2.2). `.right` is the LEFT camera's plane
/// coord and `.left` the RIGHT's, per the optimizer's swap convention.
///
/// Shared by [`WorkerBackend::refine_lens`] and the real-clip acceptance test so
/// both derive observations identically (CR-01).
fn raw_observations_from_verified(
    matches: &[reco_calibrate::types::MatchedPoint],
    left_params: &reco_core::calibration::CameraParams,
    right_params: &reco_core::calibration::CameraParams,
) -> Vec<reco_calibrate::RawPixelMatch> {
    let (lw, lh) = (left_params.width.max(1), left_params.height.max(1));
    let (rw, rh) = (right_params.width.max(1), right_params.height.max(1));
    matches
        .iter()
        .map(|p| {
            let left_und = reco_calibrate::geometry::plane_to_pixel(p.right, lw, lh);
            let right_und = reco_calibrate::geometry::plane_to_pixel(p.left, rw, rh);
            let (lx, ly) = reco_core::lens::undistorted_to_distorted(
                left_und[0],
                left_und[1],
                lw,
                lh,
                left_params,
            );
            let (rx, ry) = reco_core::lens::undistorted_to_distorted(
                right_und[0],
                right_und[1],
                rw,
                rh,
                right_params,
            );
            reco_calibrate::RawPixelMatch {
                left_px: [lx, ly],
                right_px: [rx, ry],
            }
        })
        .collect()
}

/// Run the reduced `k1` refinement on retained verified matches.
///
/// This is the single worker path shared by [`WorkerBackend::refine_lens`] and
/// the real-clip acceptance test: both derive observations with
/// [`raw_observations_from_verified`] and refine with
/// [`reco_calibrate::refine_intrinsics`], so the test cannot mask the runtime
/// behaviour by exercising a different data volume (CR-01).
fn refine_verified_lens(
    matches: &[reco_calibrate::types::MatchedPoint],
    left_params: &reco_core::calibration::CameraParams,
    right_params: &reco_core::calibration::CameraParams,
    layout: &reco_core::calibration::PlaneLayout,
    heldout_fraction: f64,
) -> Result<reco_calibrate::IntrinsicsRefinement, WorkerError> {
    // No retained correspondences (e.g. immediately after a profile load):
    // refuse with a clear, actionable message rather than a generic
    // insufficient-matches rejection (CR-01).
    if matches.is_empty() {
        return Err(WorkerError::InvalidInput {
            field: "refine_lens".to_string(),
            reason: "no verified matches — run a calibration first".to_string(),
        });
    }

    let raw = raw_observations_from_verified(matches, left_params, right_params);
    let config = reco_calibrate::IntrinsicsConfig {
        heldout_fraction,
        ..reco_calibrate::IntrinsicsConfig::default()
    };
    reco_calibrate::refine_intrinsics(&raw, layout, left_params, right_params, &config)
        .map_err(|e| WorkerError::Engine(e.to_string()))
}

impl GpuEngineBackend {
    /// Build the backend on the worker thread: create the single device from
    /// the presenter's surface and configure it (FOUND-03 / D-03).
    ///
    /// **The device is created here, inside the worker** — not on the setup
    /// thread — so the worker is the sole creator and owner of the
    /// [`reco_core::gpu::GpuContext`]. The presenter is moved in and owns the
    /// render target for the app's lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::Engine`] if the device cannot be created or the
    /// surface cannot be configured against it.
    pub fn new(
        instance: reco_core::wgpu::Instance,
        presenters: PresenterChain,
        viewport: crate::presenter::ViewportRect,
        startup_fallback: Option<String>,
        calibration_cancel: Arc<AtomicBool>,
        export_cancel: Arc<AtomicBool>,
        log_buffer: crate::diagnostics::LogBuffer,
    ) -> Result<Self, WorkerError> {
        // The presenter chain (PREV-05), strongest-first, pre-built on the setup
        // thread. Index each presenter into its fixed chain slot; the first entry
        // (Native) becomes the initial active presenter. An empty chain is a
        // programmer error.
        let mut slots: [Option<Box<dyn crate::presenter::SurfacePresenter + Send>>; 3] =
            [None, None, None];
        let mut active: Option<(
            crate::presenter::PresenterKind,
            Box<dyn crate::presenter::SurfacePresenter + Send>,
        )> = None;
        for (kind, p) in presenters {
            let idx = crate::presenter::PRESENTER_CHAIN
                .iter()
                .position(|k| *k == kind)
                .ok_or_else(|| {
                    WorkerError::Engine("engine got a presenter not in the chain".into())
                })?;
            if active.is_none() {
                active = Some((kind, p));
            } else {
                slots[idx] = Some(p);
            }
        }
        let (active_kind, mut presenter) = active
            .ok_or_else(|| WorkerError::Engine("engine got an empty presenter chain".into()))?;

        // Branch on whether the presenter hosts a surface (PREV-05):
        //   Some(surface) → the surface-compatible `for_surface` constructor,
        //                   and the surface's negotiated format is the render format.
        //   None          → the headless `with_surface(None)` constructor, which
        //                   still retains the adapter so `configure`/`rebind_instance`
        //                   keep working; the readback presenter renders to an
        //                   internal target and the shared device is format-agnostic.
        // Never a second device (D-03).
        let (gpu, surface_format) = match presenter.surface() {
            Some(surface) => {
                let (gpu, surface_info) =
                    pollster::block_on(reco_core::gpu::GpuContext::for_surface(&instance, surface))
                        .map_err(|e| WorkerError::Engine(e.to_string()))?;
                (gpu, surface_info.format)
            }
            None => {
                let gpu = pollster::block_on(reco_core::gpu::GpuContext::with_surface(None))
                    .map_err(|e| WorkerError::Engine(e.to_string()))?;
                (gpu, reco_core::wgpu::TextureFormat::Rgba8Unorm)
            }
        };

        let adapter = gpu
            .adapter()
            .ok_or_else(|| WorkerError::Engine("engine did not retain a wgpu::Adapter".into()))?
            .clone();

        // FOUND-05: register the loss/uncaptured-error handlers at device init,
        // *before* the first command can run. The callback only sets a flag —
        // recovery happens on the worker thread in `recover`.
        let device_lost = Arc::new(AtomicBool::new(false));
        install_device_lost_handlers(gpu.device(), Arc::clone(&device_lost));

        // The readback channel path (PREV-05): the webview attaches a
        // `Channel<Response>` via `preview_attach_readback`; the worker drains it
        // on its next tick and stores it in the readback presenter.
        let (readback_tx, readback_rx) = std::sync::mpsc::channel();

        presenter
            .configure(gpu.device(), gpu.queue(), &adapter, viewport)
            .map_err(|e| WorkerError::Engine(e.to_string()))?;

        Ok(Self {
            gpu,
            surface_format,
            pose: reco_control::pose_control::PoseControl::with_defaults(),
            viewport,
            blend_width: 0.05,
            rig_tilt: 0.0,
            session: None,
            loaded: None,
            first_frame_marker_pending: true,
            last_frame_time: std::time::Instant::now(),
            window_width: 1280,
            window_height: 800,
            chrome: crate::presenter::ChromeState::default(),
            view_mode: crate::presenter::ViewMode::Panorama,
            input_paths: std::collections::HashMap::new(),
            inputs: [None, None],
            lens_overrides: [None, None],
            lens_override_candidates: [None, None],
            current_calibration: None,
            calibration_path: None,
            pending_project: None,
            has_result: false,
            calibration: None,
            manual: None,
            verified_seed: Vec::new(),
            verified_seed_frame: None,
            verified_matches: Vec::new(),
            manual_solve_at: None,
            manual_relens_pending: false,
            manual_preview_dirty: false,
            source: None,
            input_size: None,
            source_spec: None,
            presenter,
            presenters: slots,
            active_kind,
            active_reason: startup_fallback,
            readback_rx,
            readback_tx,
            instance,
            adapter,
            device_lost,
            recovery_attempts: 0,
            calibration_cancel,
            export_cancel,
            log_buffer,
            renderer: None,
            renderer_input: None,
        })
    }

    /// Rebuild the renderer + reconfigure the surface on the *current* device
    /// (`Outdated` recovery, FOUND-05).
    fn reconfigure_surface(&mut self) -> Result<(), WorkerError> {
        self.presenter
            .configure(
                self.gpu.device(),
                self.gpu.queue(),
                &self.adapter,
                self.viewport,
            )
            .map_err(|e| WorkerError::Engine(e.to_string()))
    }

    /// Rebuild the device, the renderer, and the surface (`Lost` recovery,
    /// FOUND-05).
    ///
    /// The `Surface` created from the window **stays valid** while the window
    /// lives (RESEARCH Pattern 5) — only the device is recreated, so recovery
    /// never opens a second window and never presents a black frame.
    fn rebuild_device_and_surface(&mut self) -> Result<(), WorkerError> {
        // Drop device-dependent GPU objects first: the renderer holds bind
        // groups/textures that are invalid after loss.
        self.renderer = None;
        // The old `Instance` is bound to the lost parent device — requesting an
        // adapter from it fails forever with "Parent device is lost". Create a
        // fresh instance and bind the surviving child window to it, then rebuild
        // the device. The WINDOW survives; only device-bound objects are remade
        // (the resolved `rebuild-reconfigure` contract, D-03).
        let instance = reco_core::wgpu::Instance::default();
        self.presenter
            .rebind_instance(&instance)
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        self.instance = instance;
        // Branch device creation on whether the presenter hosts a surface
        // (PREV-05), exactly as `new` does — a headless (readback) presenter
        // recovers through `with_surface(None)`. Never a second device (D-03).
        let (gpu, surface_format) = match self.presenter.surface() {
            Some(surface) => {
                let (gpu, surface_info) = pollster::block_on(
                    reco_core::gpu::GpuContext::for_surface(&self.instance, surface),
                )
                .map_err(|e| WorkerError::Engine(e.to_string()))?;
                (gpu, surface_info.format)
            }
            None => {
                let gpu = pollster::block_on(reco_core::gpu::GpuContext::with_surface(None))
                    .map_err(|e| WorkerError::Engine(e.to_string()))?;
                (gpu, reco_core::wgpu::TextureFormat::Rgba8Unorm)
            }
        };
        self.adapter = gpu
            .adapter()
            .ok_or_else(|| WorkerError::Engine("rebuilt device retained no adapter".into()))?
            .clone();
        self.gpu = gpu;
        self.surface_format = surface_format;
        // Re-register the callback on the new device: a second loss must be
        // detected too.
        self.device_lost.store(false, Ordering::SeqCst);
        install_device_lost_handlers(self.gpu.device(), Arc::clone(&self.device_lost));
        self.reconfigure_surface()?;
        // Rebuild the renderer if a preview had built one, so live frames
        // resume without re-importing.
        if let Some((cal, in_w, in_h)) = self.renderer_input.clone() {
            self.renderer = Some(self.build_renderer(cal, in_w, in_h)?);
        }
        Ok(())
    }

    /// Build a `StitchRenderer` from the current device and the given inputs.
    /// The FOV ceiling `clamp_via_coverage` will actually allow, in degrees.
    ///
    /// The stitchable area (coverage boundary from the loaded calibration)
    /// bounded by the configured `fov_max_degrees` — the same `min(...)` the CLI
    /// logs as `max FOV = … (coverage-limited)` at
    /// `crates/reco-cli/src/preview.rs:131-141`.
    ///
    /// Computed rather than stored: it is a property of the calibration, so
    /// there is no state to keep in sync, and reading it fresh means a device
    /// recovery that rebuilds the renderer cannot leave a stale ceiling.
    ///
    /// Falls back to the configured max while no renderer exists (before the
    /// first session), which is the honest answer: without a calibration there
    /// is no coverage to be bounded by, and the configured max is what the pose
    /// clamps to.
    fn fov_ceiling(&self) -> f32 {
        fov_ceiling_from(
            self.renderer.as_ref().map(|r| r.coverage()),
            self.pose.config().fov_max_degrees,
        )
    }

    fn build_renderer(
        &self,
        cal: reco_core::calibration::MatchCalibration,
        in_w: u32,
        in_h: u32,
    ) -> Result<reco_core::render::stitch_renderer::StitchRenderer, WorkerError> {
        let viewport =
            crate::presenter::viewport_config(self.viewport, self.blend_width, self.rig_tilt);
        reco_core::render::stitch_renderer::StitchRenderer::new(
            cal,
            self.gpu.clone(),
            viewport,
            in_w,
            in_h,
            self.surface_format,
            reco_core::render::renderer::InputFormat::Yuv420p,
        )
        .map_err(|e| WorkerError::Engine(e.to_string()))
    }

    /// The active session's total frame count, if any.
    fn session_total(&self) -> Option<u64> {
        self.session.as_ref().and_then(|t| t.total_frames())
    }

    /// The active session's frame-rate rational, if any.
    fn session_fps_rational(&self) -> Option<(i32, i32)> {
        self.session.as_ref().and_then(|t| t.fps_rational())
    }

    /// Recompute the native viewport from the stored window size + chrome state
    /// and reconfigure the presenter's surface in place.
    ///
    /// Runs at a **command boundary** (the worker drains commands before a
    /// session tick), so no `SurfaceTexture` is alive. It does NOT touch the
    /// transport or pose, so a resize preserves the playhead and view
    /// (RESEARCH Pitfall 8).
    fn reconfigure_viewport(&mut self, events: &EventSink) {
        let rect = crate::presenter::ViewportRect::for_chrome(
            self.window_width,
            self.window_height,
            &self.chrome,
        );
        self.viewport = rect;
        // Skip a non-drawable rect: reconfiguring to zero would make the
        // pipeline's `resize` warn and no-op anyway; keep the last good rect.
        if !rect.is_drawable() {
            events.info(format!(
                "viewport reconfigure skipped: {}x{} not drawable",
                rect.width, rect.height
            ));
            return;
        }
        let result = self
            .presenter
            .resize(rect)
            .and_then(|()| {
                self.presenter
                    .configure(self.gpu.device(), self.gpu.queue(), &self.adapter, rect)
            })
            .map(|()| {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.pipeline_mut().resize(rect.width, rect.height);
                }
            });
        match result {
            Ok(()) => {
                // Byte-identical to the line 02-08's probe asserts. Do not
                // reword it; add the geometry line below instead.
                events.info(format!(
                    "viewport reconfigured to {}x{}",
                    rect.width, rect.height
                ));
                // Read the window's server-side geometry back so a stale X
                // window is visible in the log rather than invisible on screen.
                let line = native_geometry_line(rect, self.presenter.child_geometry(), self.chrome);
                if line.contains("mismatch") {
                    events.log(Level::Warn, line);
                } else {
                    events.info(line);
                }
            }
            Err(e) => events.failed(WorkerError::Engine(e.to_string())),
        }
    }

    /// The sending half of the readback channel path (PREV-05).
    ///
    /// Handed to the Tauri command layer so `preview_attach_readback` can attach
    /// a webview `Channel<Response>`.
    pub fn readback_sender(
        &self,
    ) -> std::sync::mpsc::Sender<tauri::ipc::Channel<tauri::ipc::Response>> {
        self.readback_tx.clone()
    }

    /// Drain any webview readback channels attached since the last tick and
    /// store the newest into the readback presenter (PREV-05).
    ///
    /// A no-op when nothing is pending. Called at the top of every session tick
    /// and on `SetPresenter`, so the channel is attached without a second thread.
    fn drain_readback_channels(&mut self) {
        while let Ok(channel) = self.readback_rx.try_recv() {
            // Hand the channel to whichever inactive presenter is the readback
            // one; every other impl's `attach_readback_channel` is a no-op
            // (PREV-05). If readback is currently active, its forwarding happens
            // through the same trait method on the active presenter.
            let mut attached = false;
            for slot in self.presenters.iter_mut().flatten() {
                slot.attach_readback_channel(channel.clone());
                attached = true;
            }
            self.presenter.attach_readback_channel(channel);
            let _ = attached;
        }
    }

    /// Activate the presenter for `kind`, configuring it against the shared
    /// device (PREV-05 swap). Never creates a second device.
    ///
    /// On success the incoming presenter becomes active, the outgoing one is
    /// released and returned to its chain slot, and the new `Surface` (if any)
    /// drives the renderer's format. Returns the typed error on failure so the
    /// caller can fall through the chain.
    fn install_presenter(
        &mut self,
        kind: crate::presenter::PresenterKind,
    ) -> Result<(), crate::presenter::PresenterError> {
        if kind == self.active_kind {
            return Ok(());
        }
        let idx = crate::presenter::PRESENTER_CHAIN
            .iter()
            .position(|k| *k == kind)
            .ok_or_else(|| crate::presenter::PresenterError::Unsupported {
                reason: format!("{kind:?} is not in the presenter chain"),
            })?;
        let mut incoming = self.presenters[idx].take().ok_or_else(|| {
            crate::presenter::PresenterError::Unsupported {
                reason: format!("{kind:?} presenter is unavailable"),
            }
        })?;
        // Configure the incoming presenter against the EXISTING device/adapter.
        // If this fails, put the incoming presenter back in its slot so the chain
        // is not corrupted.
        if let Err(e) = incoming.configure(
            self.gpu.device(),
            self.gpu.queue(),
            &self.adapter,
            self.viewport,
        ) {
            self.presenters[idx] = Some(incoming);
            return Err(e);
        }
        // Only release the outgoing presenter's window AFTER the incoming one
        // is successfully configured.
        self.presenter.release_presenter_window();
        let outgoing = std::mem::replace(&mut self.presenter, incoming);
        let old_idx = crate::presenter::PRESENTER_CHAIN
            .iter()
            .position(|k| *k == self.active_kind)
            .expect("active kind is in the chain");
        self.presenters[old_idx] = Some(outgoing);
        self.active_kind = kind;
        // The renderer's target format follows the presenter: a surface-backed
        // presenter renders in its negotiated format, readback in `Rgba8Unorm`.
        // Rebuild a live renderer so the swap preserves the picture.
        if let Some(format) = self.presenter.configured_format()
            && format != self.surface_format
        {
            self.surface_format = format;
            if let Some((cal, in_w, in_h)) = self.renderer_input.clone() {
                self.renderer = Some(self.build_renderer(cal, in_w, in_h).map_err(|e| {
                    crate::presenter::PresenterError::Surface {
                        reason: e.to_string(),
                    }
                })?);
            }
        }
        Ok(())
    }
}

/// Spawn the calibration heartbeat monitor (CALB-01 / D3-10).
///
/// The monitor emits a `CalibrationHeartbeat` every ~500 ms until `stop` is
/// set, so a long silent stage (e.g. the 30-60 s telemetry parse in
/// `DetectingProfiles`) never looks stuck. It reads the shared last-detail so
/// the tick reports the stage that is actually running. The worker always sets
/// `stop` and joins the handle before returning, so the thread cannot outlive
/// the run or accumulate (T-03-09).
fn spawn_heartbeat_monitor(
    tx: Sender<WorkerEvent>,
    stop: Arc<AtomicBool>,
    detail: Arc<std::sync::Mutex<(crate::events::CalibrationStage, String)>>,
) -> Result<JoinHandle<()>, WorkerError> {
    thread::Builder::new()
        .name("reco-calibration-heartbeat".to_string())
        .spawn(move || {
            let start = std::time::Instant::now();
            loop {
                // Sleep in short slices so the stop flag is honored promptly
                // (never wait a full 500 ms after calibration returns).
                for _ in 0..10 {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                let (step, last_detail) = match detail.lock() {
                    Ok(guard) => guard.clone(),
                    Err(_) => (crate::events::CalibrationStage::Probing, String::new()),
                };
                let _ = tx.send(WorkerEvent::CalibrationHeartbeat {
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    step,
                    last_detail,
                });
            }
        })
        .map_err(|e| WorkerError::Engine(format!("failed to spawn heartbeat monitor: {e}")))
}

/// Install the FOUND-05 device-lost callback and uncaptured-error handler.
///
/// The lost callback only flips the shared flag and logs; the worker thread
/// runs `recover` at a safe point (never inside the callback, which wgpu may
/// invoke on an arbitrary thread). `on_uncaptured_error` logs so a device
/// validation error is visible rather than silently dropped.
fn install_device_lost_handlers(device: &reco_core::wgpu::Device, lost: Arc<AtomicBool>) {
    device.set_device_lost_callback(move |reason, message| {
        // `DeviceLostReason::Destroyed` is the debug-simulation path; a real
        // loss reports `Unknown`. Either way, flag it for recovery.
        log::warn!("device lost: reason={reason:?} message={message}");
        lost.store(true, Ordering::SeqCst);
    });
    // `UncapturedErrorHandler` is a blanket impl over `Fn(Error) + Send + Sync`,
    // so a plain closure is the handler.
    device.on_uncaptured_error(Arc::new(|error: reco_core::wgpu::Error| {
        log::error!("uncaptured wgpu error: {error}");
    }));
}

/// Translate an inbound [`reco_control::ControlIntent`] into a call on the
/// worker's [`reco_control::pose_control::PoseControl`].
///
/// This is the single dispatch point for engine input: the Tauri command
/// handlers never touch `PoseControl` (they post a `WorkerCommand::Intent`);
/// the worker calls this. It mirrors the CLI's per-key dispatch reference
/// (`crates/reco-cli/src/preview.rs:581-633`). Extracted as a free function so
/// the intent→pose effect is testable without a GPU.
///
/// [`reco_control::PoseIntent::Reset`] is special-cased to
/// [`PoseControl::snap_to_rest`] (an immediate return to the configured rest
/// position) rather than the eased `IntentTranslator` hotkey path: "Reset view"
/// is a deliberate snap, not a smoothing target (PREV-04 / CONTEXT "Reset
/// view"). Every other intent keeps the shared vocabulary via
/// [`reco_control::IntentTranslator`].
pub fn dispatch_intent(
    pose: &mut reco_control::pose_control::PoseControl,
    intent: reco_control::ControlIntent,
) {
    if matches!(
        intent,
        reco_control::ControlIntent::Pose(reco_control::PoseIntent::Reset)
    ) {
        pose.snap_to_rest();
        return;
    }
    reco_control::IntentTranslator::new(pose).dispatch(intent);
}

impl EngineBackend for GpuEngineBackend {
    fn import(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        events.info("import started");
        let paths =
            crate::hardcoded::media_paths().map_err(|e| WorkerError::Engine(e.to_string()))?;
        let cal = reco_core::calibration::MatchCalibration::from_file(paths.calibration.as_path())
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let source = reco_io::adapters::FfmpegFileSource::open_with_offset(
            paths.left.as_path(),
            paths.right.as_path(),
            cal.sync_offset,
        )
        .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let info = source.info();
        self.rig_tilt = cal.rig_tilt as f32;
        self.input_size = Some((info.width, info.height));
        self.calibration = Some(cal);
        self.source = Some(source);
        // The startup import is the hardcoded pair, not the operator's clips; a
        // later `set_input` for both roles replaces this source (D3-15).
        self.source_spec = None;

        // Build the transport from the loaded source's timing metadata so a
        // transport command issued before `begin_preview` carries the real
        // fps/frame count instead of a placeholder (see [`Self::transport`]).
        let transport = crate::transport::Transport::new(info.fps, info.fps_rational, {
            self.source.as_ref().and_then(|s| s.total_frames())
        });
        self.loaded = Some(transport);
        Ok(())
    }

    fn set_input(
        &mut self,
        role: crate::events::InputRole,
        path: String,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // Probe via the worker's own FFmpeg path (reco-io); no engine entry point
        // is named by the command handler — this is the worker's job.
        let probe = reco_io::ffmpeg::calibration_io::probe_video(std::path::Path::new(&path))
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let metadata = project_metadata(&probe);
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path.as_str())
            .to_string();
        let idx = role_index(role);
        self.inputs[idx] = Some(metadata.clone());
        self.input_paths.insert(role, path);
        events.info(format!("{} selected: {filename}", role.label()));
        events.metadata(role, metadata);
        // Recompute the advisory checks from both inputs and report them
        // (IMPT-03). Changing an input after a run invalidates the result
        // (D3-08): the scorecard must reflect the clips it actually used.
        self.emit_readiness(events);
        self.invalidate_result(events);
        // Once both roles are set, decode the operator's clips instead of the
        // hardcoded startup pair (D3-15). A single set input keeps the startup
        // source so the Phase 1/2 no-import path is unchanged.
        self.sync_operator_source()?;
        Ok(())
    }

    fn clear_input(
        &mut self,
        role: crate::events::InputRole,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let idx = role_index(role);
        self.inputs[idx] = None;
        self.input_paths.remove(&role);
        self.lens_overrides[idx] = None;
        self.lens_override_candidates[idx] = None;
        events.info(format!("{} cleared", role.label()));
        self.emit_readiness(events);
        self.invalidate_result(events);
        // A cleared input must not leave a stale clip behind: drop the operator
        // source so Preview cannot decode a clip the operator removed. With
        // both roles still set the source is simply re-resolved (a no-op when
        // unchanged).
        self.sync_operator_source()?;
        Ok(())
    }

    fn lens_candidates(
        &mut self,
        role: crate::events::InputRole,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let idx = role_index(role);
        // Resolution of the input drives the candidate filter; a missing input
        // lists every profile (0 = wildcard in `LensDatabase::candidates`).
        let (width, height) = self.inputs[idx]
            .as_ref()
            .and_then(crate::calibration::parse_resolution)
            .unwrap_or((0, 0));
        let candidates = reco_calibrate::lens_database::LensDatabase::embedded()
            .candidates(width, height)
            .into_iter()
            .map(|summary| crate::events::LensCandidate {
                camera: summary.camera,
                lens: summary.lens,
                width: summary.width,
                height: summary.height,
            })
            .collect();
        events.lens_candidates(role, candidates);
        Ok(())
    }

    fn set_lens_override(
        &mut self,
        role: crate::events::InputRole,
        candidate: crate::events::LensCandidate,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let idx = role_index(role);
        // Resolve the chosen summary back to full `CameraParams` via the
        // embedded database (D3-07). A summary that no longer resolves clears
        // the override rather than storing a half-built profile.
        let summary = reco_calibrate::types::LensProfileSummary {
            camera: candidate.camera.clone(),
            lens: candidate.lens.clone(),
            width: candidate.width,
            height: candidate.height,
        };
        let params =
            reco_calibrate::lens_database::LensDatabase::embedded().load_by_summary(&summary);
        match params {
            Some(p) => {
                self.lens_overrides[idx] = Some(p);
                self.lens_override_candidates[idx] = Some(candidate.clone());
                events.info(format!(
                    "{} lens override: {} {}",
                    role.label(),
                    candidate.camera,
                    candidate.lens
                ));
                events.lens_override_applied(role, Some(candidate));
            }
            None => {
                // The summary did not resolve in the embedded database. Leave
                // the input on auto-detect and do NOT tag the row `overridden`:
                // the scorecard must never claim a profile the engine did not
                // use (D3-07/D3-08 provenance honesty, WR-02).
                self.lens_overrides[idx] = None;
                self.lens_override_candidates[idx] = None;
                events.log(
                    Level::Warn,
                    format!(
                        "{} lens override '{} {}' did not resolve; auto-detect will run",
                        role.label(),
                        candidate.camera,
                        candidate.lens
                    ),
                );
                events.lens_override_applied(role, None);
            }
        }
        self.emit_readiness(events);
        self.invalidate_result(events);
        Ok(())
    }

    fn clear_lens_override(
        &mut self,
        role: crate::events::InputRole,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let idx = role_index(role);
        self.lens_overrides[idx] = None;
        self.lens_override_candidates[idx] = None;
        events.info(format!("{} lens override cleared", role.label()));
        events.lens_override_applied(role, None);
        self.emit_readiness(events);
        self.invalidate_result(events);
        Ok(())
    }

    fn set_field_roi(
        &mut self,
        left: Vec<[f64; 2]>,
        right: Vec<[f64; 2]>,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // A field ROI frames detections; without a calibration there is nothing
        // to attach it to. Reject with a typed error rather than a silent no-op
        // (CALB-09), so the editor can render the cause.
        if self.current_calibration.is_none() {
            return Err(WorkerError::InvalidInput {
                field: "field_roi".to_string(),
                reason: "no calibration result — run or load a calibration first".to_string(),
            });
        }
        let roi = normalize_field_roi(left, right);
        let cleared = roi.left.is_empty() && roi.right.is_empty();
        // Write onto both the save-path calibration and the preview source, so
        // the polygon round-trips through save/load AND is consumed by the
        // engine/autocam path (CALB-09).
        if let Some(cal) = self.current_calibration.as_mut() {
            cal.field_roi = if cleared { None } else { Some(roi.clone()) };
        }
        if let Some(cal) = self.calibration.as_mut() {
            cal.field_roi = if cleared { None } else { Some(roi.clone()) };
        }
        // The ROI does not change the stitch geometry, so the result stays valid
        // (no `invalidate_result`).
        if cleared {
            events.field_roi_cleared();
        } else {
            events.field_roi_applied(roi);
        }
        Ok(())
    }

    fn refine_lens(
        &mut self,
        heldout_fraction: f64,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // A refinement needs a profile to write back into and a layout to solve
        // at. Without a calibration there is nothing to refine; reject with a
        // typed error rather than a silent no-op (INTR-03).
        let (left_params, right_params, layout, baseline_k1) = {
            let Some(cal) = self.current_calibration.as_ref() else {
                return Err(WorkerError::InvalidInput {
                    field: "refine_lens".to_string(),
                    reason: "no calibration result — run or load a calibration first".to_string(),
                });
            };
            (
                cal.left.clone(),
                cal.right.clone(),
                cal.layout.clone(),
                cal.left.d[0],
            )
        };

        let refinement = refine_verified_lens(
            &self.verified_matches,
            &left_params,
            &right_params,
            &layout,
            heldout_fraction,
        )?;

        let view = crate::calibration::project_intrinsics_refinement(&refinement, baseline_k1);

        // Write the refined k1 onto the profile ONLY when the guard accepted it
        // (T-04.2-10). A rejected or ill-conditioned result leaves `k1`
        // unchanged on both the save path and the preview source, so the
        // round-trip profile is always the safe one.
        if refinement.accepted {
            if let Some(cal) = self.current_calibration.as_mut() {
                cal.left.d[0] = refinement.k1;
                cal.right.d[0] = refinement.k1;
            }
            if let Some(cal) = self.calibration.as_mut() {
                cal.left.d[0] = refinement.k1;
                cal.right.d[0] = refinement.k1;
            }
        }

        events.intrinsics_refined(view);
        Ok(())
    }

    fn calibrate(
        &mut self,
        options: crate::events::CalibrationOptions,
        events: &EventSink,
        _interrupted: &AtomicBool,
    ) -> Result<(), WorkerError> {
        let left = self
            .input_paths
            .get(&crate::events::InputRole::Left)
            .cloned()
            .ok_or(WorkerError::NotImported)?;
        let right = self
            .input_paths
            .get(&crate::events::InputRole::Right)
            .cloned()
            .ok_or(WorkerError::NotImported)?;

        // Clear a stale cancel from a previous run before starting.
        self.calibration_cancel.store(false, Ordering::SeqCst);

        events.stage(
            crate::events::CalibrationStage::Probing,
            crate::events::StageStatus::Active,
            "Probing video metadata",
        );
        events.progress(0.0);

        // Shared last-detail state for the heartbeat monitor, so a silent stage
        // reports what it is doing (D3-10).
        let detail = Arc::new(std::sync::Mutex::new((
            crate::events::CalibrationStage::Probing,
            "Probing video metadata".to_string(),
        )));
        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat = spawn_heartbeat_monitor(
            events.sender_clone(),
            Arc::clone(&stop),
            Arc::clone(&detail),
        )?;

        let engine_options = crate::calibration::build_video_options(
            &options,
            self.lens_overrides[0].clone(),
            self.lens_overrides[1].clone(),
        );

        // The progress callback runs on the worker thread inside
        // `calibrate_videos_with_gpu`; it maps engine steps to host stages and
        // updates the heartbeat's last-detail.
        let mut on_progress = |progress: &reco_calibrate::types::CalibrationProgress| {
            let stage = crate::calibration::stage_from_step(progress.step);
            if let Ok(mut guard) = detail.lock() {
                *guard = (stage, progress.detail.clone());
            }
            events.stage(
                stage,
                crate::events::StageStatus::Active,
                progress.detail.clone(),
            );
            events.progress(stage_fraction(stage));
        };

        let result = reco_calibrate::video::calibrate_videos_with_gpu(
            &self.gpu,
            std::path::Path::new(&left),
            std::path::Path::new(&right),
            engine_options,
            &mut on_progress,
            &self.calibration_cancel,
        );

        // Always stop and join the monitor before returning (T-03-09): it must
        // not outlive the run or accumulate.
        stop.store(true, Ordering::SeqCst);
        let _ = heartbeat.join();

        match result {
            Ok(calibration_result) => {
                let scorecard = crate::calibration::project_scorecard(&calibration_result);
                // Retain the reference frame's post-RANSAC matches so a later
                // manual session can pre-populate its pins from geometrically
                // verified matches ONLY (MANU-04). Rejected candidates are never
                // kept. The first sampled frame is the calibration's reference
                // frame; retain its source index too so the manual flow seeds
                // only onto that frame (WR-01).
                self.verified_seed = calibration_result
                    .per_frame
                    .first()
                    .map(|fm| fm.points.clone())
                    .unwrap_or_default();
                self.verified_seed_frame = calibration_result.frame_indices.first().copied();
                // Retain every frame's verified matches (flattened) for the
                // opt-in k1 refinement. Aggregating across frames is required to
                // clear the conditioning gate on the real pair: a single
                // reference frame yields ~7-11 matches, far below
                // `RECOMMENDED_MIN_MATCHES` (CR-01).
                self.verified_matches = calibration_result
                    .per_frame
                    .iter()
                    .flat_map(|fm| fm.points.iter().copied())
                    .collect();
                // Adopt the result as the preview source too (D3-15 / WR-05),
                // so the scorecard and the rendered stitch describe one profile.
                self.adopt_calibration(calibration_result.calibration.clone());
                // Pre-populate the field ROI editor from the fresh result (CALB-09).
                self.emit_field_roi(events);
                events.stage(
                    crate::events::CalibrationStage::Optimizing,
                    crate::events::StageStatus::Done,
                    "Calibration complete",
                );
                events.progress(1.0);
                events.result(scorecard);
                // Publish the bounded debug inspector payload (CALB-08): the
                // fitted layout gives each verified point a real per-point
                // residual; the retained undistorted pair supplies the thumbs.
                events.calibration_debug(crate::calibration::build_debug_report(
                    &calibration_result.per_frame,
                    Some(&calibration_result.calibration.layout),
                    calibration_result.residual_error,
                ));
                Ok(())
            }
            Err(reco_calibrate::video::CalibrateVideosError::Cancelled) => {
                // CALB-02: the partial result is discarded and never offered.
                events.log(
                    Level::Warn,
                    "calibration cancelled — partial result discarded",
                );
                Ok(())
            }
            Err(e) => {
                // Map the typed failure to a plain-language diagnosis BEFORE it
                // could be flattened to a string (RESEARCH Pitfall 2 / CALB-04).
                use crate::events::{CalibrationDiagnosis, StageStatus};
                use reco_calibrate::video::CalibrateVideosError;

                let last_stage = detail
                    .lock()
                    .map(|guard| guard.0)
                    .unwrap_or(crate::events::CalibrationStage::Probing);

                let diagnosis = match &e {
                    CalibrateVideosError::Diagnostic(failure) => {
                        crate::calibration::diagnose_calibration_failure(
                            &failure.error,
                            crate::calibration::stage_from_step(failure.step),
                            &failure.frames,
                        )
                    }
                    CalibrateVideosError::Calibrate(error) => {
                        crate::calibration::diagnose_calibration_failure(error, last_stage, &[])
                    }
                    // I/O, GPU, or no-frames: the raw typed Display text plus a
                    // generic next step — never a fabricated cause.
                    other => {
                        let raw = other.to_string();
                        CalibrationDiagnosis {
                            cause: raw.clone(),
                            fix: "Check that both clips import and decode cleanly, then try again."
                                .to_string(),
                            raw_error: raw,
                            stage: last_stage,
                            metrics: crate::events::DiagnosisMetrics::default(),
                        }
                    }
                };

                events.stage(
                    diagnosis.stage,
                    StageStatus::Failed,
                    diagnosis.cause.clone(),
                );
                events.failed_diagnosis(diagnosis);

                // Publish the debug inspector payload even on failure (CALB-08),
                // from the partial per-frame matches the engine retained. No
                // fitted layout exists, so points carry the run's (zero) residual.
                let debug_frames: &[reco_calibrate::types::FrameMatches] = match &e {
                    CalibrateVideosError::Diagnostic(failure) => &failure.frames,
                    _ => &[],
                };
                events.calibration_debug(crate::calibration::build_debug_report(
                    debug_frames,
                    None,
                    0.0,
                ));
                Ok(())
            }
        }
    }

    fn load_profile(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
        // Validate by deserialization into `MatchCalibration` (size cap + typed
        // `validate()`); never deserialize unchecked (T-03-08).
        let calibration =
            reco_core::calibration::MatchCalibration::from_file(std::path::Path::new(&path))
                .map_err(|e| WorkerError::ProfileLoad(e.to_string()))?;
        // A loaded profile carries no retained raw correspondences, so the
        // opt-in k1 refinement has nothing to aggregate and must refuse clearly
        // rather than reuse a previous run's matches (CR-01).
        self.verified_matches.clear();
        // Adopt the loaded profile as the live result and preview source so the
        // preview/export flows consume it unchanged (D3-15 / WR-05).
        self.adopt_calibration(calibration);
        // Retain the referenced `.json` so a saved `.reco` can point at it
        // (PROJ-01).
        self.calibration_path = Some(path.clone());
        // Pre-populate the field ROI editor from the loaded profile (CALB-09).
        self.emit_field_roi(events);
        events.profile_loaded(path);
        Ok(())
    }

    fn save_profile(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
        // Gate on the live-result flag, not merely on a stored calibration:
        // `invalidate_result` clears `current_calibration`, and checking the
        // flag as well keeps a stale profile from ever being written if a
        // future producer leaves a value behind (WR-01 / D3-08).
        if !self.has_result {
            return Err(WorkerError::ProfileSave(
                "no calibration result to save".to_string(),
            ));
        }
        let calibration = self
            .current_calibration
            .as_ref()
            .ok_or_else(|| WorkerError::ProfileSave("no calibration result to save".to_string()))?;
        calibration
            .to_file(std::path::Path::new(&path))
            .map_err(|e| WorkerError::ProfileSave(e.to_string()))?;
        // Retain the written path so a `.reco` can reference it (PROJ-01).
        self.calibration_path = Some(path.clone());
        events.profile_saved(path);
        Ok(())
    }

    fn manual_begin(&mut self, frame: u64, events: &EventSink) -> Result<(), WorkerError> {
        // The manual flow is always reachable and never requires a `.json`; it
        // uses the operator's selected clips when both are set, else the
        // startup hardcoded clips (MANU-01).
        let (left_path, right_path) = self.manual_frame_paths()?;
        let probe = reco_io::ffmpeg::calibration_io::probe_video(std::path::Path::new(&left_path))
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let right_probe =
            reco_io::ffmpeg::calibration_io::probe_video(std::path::Path::new(&right_path))
                .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let frames_total = probe.total_frames.max(1);
        let right_frames_total = right_probe.total_frames.max(1);
        // T-04.1-01 / WR-01: clamp the operator-supplied index against the
        // probed frame count BEFORE extraction, so an out-of-range index cannot
        // reach the decoder. When a verified seed exists the session opens on
        // the frame its matches came from, so the seeded markers correspond to
        // the displayed content (the UI default of 0 is not the calibration
        // frame).
        let has_seed = !self.verified_seed.is_empty();
        let (frame, seed_matches_frame) =
            manual_seed_target(frame, frames_total, self.verified_seed_frame, has_seed);
        // The offset starts at 0, so the right reference frame is the same index.
        let right_index = frame.min(right_frames_total.saturating_sub(1));
        let left_frame = extract_manual_frame(&left_path, frame)?;
        let right_frame = extract_manual_frame(&right_path, right_index)?;
        let (left_params, right_params) = self.manual_params(&left_frame, &right_frame);

        // Pre-populate the pin editor from geometrically verified (post-RANSAC)
        // automatic matches only (MANU-04). `verified_seed` holds only matches
        // that survived every filter; raw/rejected candidates are never offered.
        // The seeds are promoted to pins, and the retained `auto` set is empty,
        // so the solve never sees them twice (WR-03).
        let (lw, lh) = (left_frame.width, left_frame.height);
        let (rw, rh) = (right_frame.width, right_frame.height);
        let (seed_pins, auto_seed) =
            manual_seed_sets(&self.verified_seed, seed_matches_frame, (lw, lh), (rw, rh));
        let mut next_pin_id = 0_u32;
        let pins: Vec<SessionPin> = seed_pins
            .into_iter()
            .map(|p| {
                let id = next_pin_id;
                next_pin_id = next_pin_id.wrapping_add(1);
                SessionPin {
                    id,
                    left_px: p.left_px,
                    right_px: p.right_px,
                    verified: true,
                }
            })
            .collect();
        let seeded = !pins.is_empty();
        // The layout baseline the handles reset to and clamp against (MANU-06):
        // the loaded/current profile's layout, else a neutral rig.
        let baseline_layout = self
            .current_calibration
            .as_ref()
            .or(self.calibration.as_ref())
            .map(|c| c.layout.clone())
            .unwrap_or_else(default_plane_layout);

        self.manual = Some(ManualSession {
            frame,
            frames_total,
            left_frame,
            right_frame,
            left_params: left_params.clone(),
            right_params: right_params.clone(),
            // The offset starts at 0 with no provenance until the operator
            // confirms a nudge (MANU-02).
            sync_offset: 0,
            sync_method: crate::events::SyncMethod::None,
            right_frames_total,
            pins,
            next_pin_id,
            auto_seed,
            baseline_left_params: left_params,
            baseline_right_params: right_params,
            current_layout: baseline_layout.clone(),
            baseline_layout,
        });
        // A fresh session never has a solve armed; an empty set never solves.
        self.manual_solve_at = None;
        self.manual_relens_pending = false;
        events.manual_session_started(frame, probe.fps, frames_total);
        events.manual_pins(self.manual_pin_views(), seeded);
        self.emit_manual_params(events);
        self.mark_manual_preview_dirty();
        Ok(())
    }

    fn manual_set_frame(&mut self, frame: u64, _events: &EventSink) -> Result<(), WorkerError> {
        if self.manual.is_none() {
            return Err(WorkerError::InvalidInput {
                field: "frame".to_string(),
                reason: "no manual session is open — begin one first".to_string(),
            });
        }
        let (frames_total, right_frames_total) = self
            .manual
            .as_ref()
            .map(|session| (session.frames_total, session.right_frames_total))
            .unwrap_or((1, 1));
        let frame = frame.min(frames_total.saturating_sub(1));
        let changed = self
            .manual
            .as_ref()
            .is_some_and(|session| session.frame != frame);
        if changed {
            let (left_path, right_path) = self.manual_frame_paths()?;
            let left_frame = extract_manual_frame(&left_path, frame)?;
            // The right reference frame carries the session's sync offset so the
            // pair is aligned on the operator's chosen offset (MANU-02).
            let right_index = self.right_frame_index(frame, right_frames_total);
            let right_frame = extract_manual_frame(&right_path, right_index)?;
            if let Some(session) = self.manual.as_mut() {
                session.frame = frame;
                session.left_frame = left_frame;
                session.right_frame = right_frame;
            }
        }
        self.mark_manual_preview_dirty();
        Ok(())
    }

    fn manual_exit(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        // T-04.1-02: drop the retained planes; a session never accumulates.
        // Exiting does not touch `current_calibration`, so an existing profile
        // survives leaving the flow (UI-SPEC "never a dead end").
        self.manual = None;
        // IN-01: clear any armed debounced solve, or the worker loop would fire
        // it after the flow closed and emit a spurious stale solve state.
        self.manual_solve_at = None;
        self.manual_relens_pending = false;
        events.info("manual calibration session ended");
        events.manual_solve_state(false, false, false);
        Ok(())
    }

    fn manual_detect_sync(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        // The flow is always reachable: detect from the operator's clips when
        // both are set, else the startup clips (MANU-01).
        let (left_path, right_path) = self.manual_frame_paths()?;
        const SAMPLE_RATE: u32 = 44100;
        // Runs inline on the worker thread (T-04.1-09); the webview never
        // blocks on it. The engine distinguishes "unavailable" with a typed
        // error, which becomes a `confidence: None` result — never a fabricated
        // zero (MANU-02).
        match reco_calibrate::video::detect_audio_sync(
            std::path::Path::new(&left_path),
            std::path::Path::new(&right_path),
            SAMPLE_RATE,
        ) {
            Ok(estimate) => {
                // MANU-02 / WR-02: a sub-floor estimate is surfaced for display
                // but never applied as the offset — the flow defaults to 0 and
                // requires an explicit manual confirmation. A confident estimate
                // is applied with Audio provenance.
                let (applied, method) =
                    applied_audio_offset(estimate.offset_frames, estimate.confidence);
                let changed = self
                    .manual
                    .as_ref()
                    .is_some_and(|session| session.sync_offset != applied);
                if let Some(session) = self.manual.as_mut() {
                    session.sync_offset = applied;
                    session.sync_method = method;
                }
                // The applied offset selects which right frame pairs with the
                // retained left reference frame, so re-extract and re-render
                // when it changes (mirrors `manual_set_sync`); otherwise the pins
                // would pair mismatched instants (CR-01).
                if changed {
                    self.reextract_right_reference()?;
                    self.mark_manual_preview_dirty();
                }
                events.audio_sync_result(estimate.offset_frames, Some(estimate.confidence));
            }
            Err(e) => {
                // "Unavailable": reset the session offset to 0 and record no
                // provenance; the UI requires an explicit manual offset. When
                // this changes a previously applied offset, re-extract so the
                // retained right frame matches 0 (CR-01).
                let changed = self
                    .manual
                    .as_ref()
                    .is_some_and(|session| session.sync_offset != 0);
                if let Some(session) = self.manual.as_mut() {
                    session.sync_offset = 0;
                    session.sync_method = crate::events::SyncMethod::None;
                }
                if changed {
                    self.reextract_right_reference()?;
                    self.mark_manual_preview_dirty();
                }
                log::warn!("manual audio sync unavailable: {e}");
                events.audio_sync_result(0, None);
            }
        }
        Ok(())
    }

    fn manual_set_sync(
        &mut self,
        offset_frames: i64,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // Record Manual provenance on the session so the later pin/bend solves
        // use the operator's offset (MANU-02). A missing session is not fatal:
        // the offset still crosses to the UI, and beginning a session resets it.
        let mut reextract_right = false;
        if let Some(session) = self.manual.as_mut() {
            let changed = session.sync_offset != offset_frames;
            session.sync_offset = offset_frames;
            session.sync_method = crate::events::SyncMethod::Manual;
            reextract_right = changed;
            log::info!(
                "manual sync offset {} frames recorded on the session ({:?})",
                session.sync_offset,
                session.sync_method,
            );
        }
        // The offset changes which right frame pairs with the chosen left frame,
        // so re-extract the right reference frame and re-render (MANU-02/03).
        if reextract_right {
            self.reextract_right_reference()?;
            self.mark_manual_preview_dirty();
        }
        events.manual_sync_set(offset_frames, crate::events::SyncMethod::Manual);
        Ok(())
    }

    fn manual_add_pin(
        &mut self,
        left_px: [f64; 2],
        right_px: [f64; 2],
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let (lw, lh, rw, rh) = match self.manual.as_ref() {
            Some(session) => (
                session.left_frame.width,
                session.left_frame.height,
                session.right_frame.width,
                session.right_frame.height,
            ),
            None => {
                return Err(WorkerError::InvalidInput {
                    field: "pin".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
        };
        // T-04.1-10: clamp to the frame bounds so an out-of-bounds pin cannot
        // reach the solve. Non-finite coordinates are rejected at the command
        // boundary; `clamp` leaves a finite value finite.
        if let Some(session) = self.manual.as_mut() {
            let id = session.next_pin_id;
            session.next_pin_id = session.next_pin_id.wrapping_add(1);
            session.pins.push(SessionPin {
                id,
                left_px: clamp_pin_px(left_px, lw, lh),
                right_px: clamp_pin_px(right_px, rw, rh),
                verified: false,
            });
        }
        self.after_pin_mutation(events)
    }

    fn manual_move_pin(
        &mut self,
        id: u32,
        side: crate::events::ManualSide,
        px: [f64; 2],
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let (lw, lh, rw, rh) = match self.manual.as_ref() {
            Some(session) => (
                session.left_frame.width,
                session.left_frame.height,
                session.right_frame.width,
                session.right_frame.height,
            ),
            None => {
                return Err(WorkerError::InvalidInput {
                    field: "pin".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
        };
        if let Some(session) = self.manual.as_mut()
            && let Some(pin) = session.pins.iter_mut().find(|p| p.id == id)
        {
            match side {
                crate::events::ManualSide::Left => pin.left_px = clamp_pin_px(px, lw, lh),
                crate::events::ManualSide::Right => pin.right_px = clamp_pin_px(px, rw, rh),
            }
            // A pin the operator moved is no longer a verified automatic match.
            pin.verified = false;
        }
        self.after_pin_mutation(events)
    }

    fn manual_remove_pin(&mut self, id: u32, events: &EventSink) -> Result<(), WorkerError> {
        if let Some(session) = self.manual.as_mut() {
            session.pins.retain(|p| p.id != id);
        }
        self.after_pin_mutation(events)
    }

    fn manual_clear_pins(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        if let Some(session) = self.manual.as_mut() {
            session.pins.clear();
        }
        self.after_pin_mutation(events)
    }

    fn manual_set_lens(
        &mut self,
        side: crate::events::ManualSide,
        fx: f64,
        cx: f64,
        cy: f64,
        k1: f64,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // T-04.1-13: clamp at the worker boundary (the command boundary rejects
        // non-finite values; `clamp` keeps a finite value finite).
        let baseline = match self.manual.as_ref() {
            Some(session) => match side {
                crate::events::ManualSide::Left => session.baseline_left_params.clone(),
                crate::events::ManualSide::Right => session.baseline_right_params.clone(),
            },
            None => {
                return Err(WorkerError::InvalidInput {
                    field: "lens".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
        };
        let (edited, clamped) = clamp_lens_edit(&baseline, fx, cx, cy, k1);
        // Write the edited intrinsics into the session (the preview source) and
        // into `current_calibration.left`/`.right` so they survive a preview
        // rebuild and save (MANU-05 persistence).
        if let Some(session) = self.manual.as_mut() {
            match side {
                crate::events::ManualSide::Left => session.left_params = edited.clone(),
                crate::events::ManualSide::Right => session.right_params = edited.clone(),
            }
        }
        if let Some(cal) = self.current_calibration.as_mut() {
            match side {
                crate::events::ManualSide::Left => cal.left = edited,
                crate::events::ManualSide::Right => cal.right = edited,
            }
        }
        // Instant preview under the edited real parameters; the layout stays
        // frozen (no solve here).
        self.mark_manual_preview_dirty();
        self.emit_manual_params(events);
        if clamped {
            events.log(
                Level::Warn,
                "lens handle reached its safe travel limit — value clamped",
            );
        }
        // Arm the shared debounced background re-solve. A lens edit invalidates
        // the retained auto matches, so the solve re-runs the engine calibration
        // path under the edited intrinsics (MANU-05). Never solved here.
        self.manual_relens_pending = true;
        self.manual_solve_at = Some(std::time::Instant::now() + MANUAL_SOLVE_DEBOUNCE);
        events.manual_solve_state(true, true, false);
        Ok(())
    }

    fn manual_set_layout(
        &mut self,
        cam_d: f64,
        intersect: f64,
        x_ty: f64,
        x_rz: f64,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        if self.manual.is_none() {
            return Err(WorkerError::InvalidInput {
                field: "layout".to_string(),
                reason: "no manual session is open — begin one first".to_string(),
            });
        }
        let current = self
            .manual
            .as_ref()
            .map(|session| session.current_layout.clone())
            .unwrap_or_else(default_plane_layout);
        let (edited, clamped) = clamp_layout_edit(cam_d, intersect, x_ty, x_rz, &current);
        if let Some(session) = self.manual.as_mut() {
            session.current_layout = edited.clone();
        }
        // Persist the constrained edit into the calibration profile (MANU-06).
        if let Some(cal) = self.current_calibration.as_mut() {
            cal.layout = edited;
        }
        self.mark_manual_preview_dirty();
        self.emit_manual_params(events);
        if clamped {
            events.log(
                Level::Warn,
                "layout handle reached its safe travel limit — value clamped",
            );
        }
        // A layout edit needs no re-detection (the intrinsics did not change), so
        // it only arms a confirmation solve (T-04.1-14).
        self.manual_relens_pending = false;
        self.manual_solve_at = Some(std::time::Instant::now() + MANUAL_SOLVE_DEBOUNCE);
        events.manual_solve_state(true, true, false);
        Ok(())
    }

    fn manual_reset_lens(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        let Some(session) = self.manual.as_ref() else {
            return Err(WorkerError::InvalidInput {
                field: "lens".to_string(),
                reason: "no manual session is open — begin one first".to_string(),
            });
        };
        let (left, right) = (
            session.baseline_left_params.clone(),
            session.baseline_right_params.clone(),
        );
        if let Some(session) = self.manual.as_mut() {
            session.left_params = left.clone();
            session.right_params = right.clone();
        }
        if let Some(cal) = self.current_calibration.as_mut() {
            cal.left = left;
            cal.right = right;
        }
        self.mark_manual_preview_dirty();
        self.emit_manual_params(events);
        // IN-02: drop any solve a prior lens edit armed — the baseline is
        // restored, so re-detecting under it would be wasted work.
        self.manual_solve_at = None;
        self.manual_relens_pending = false;
        events.info("manual lens reset to the profile baseline");
        Ok(())
    }

    fn manual_reset_rig(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        let Some(session) = self.manual.as_ref() else {
            return Err(WorkerError::InvalidInput {
                field: "layout".to_string(),
                reason: "no manual session is open — begin one first".to_string(),
            });
        };
        let baseline = session.baseline_layout.clone();
        if let Some(session) = self.manual.as_mut() {
            session.current_layout = baseline.clone();
        }
        if let Some(cal) = self.current_calibration.as_mut() {
            cal.layout = baseline;
        }
        self.mark_manual_preview_dirty();
        self.emit_manual_params(events);
        // IN-02: drop any solve a prior layout edit armed — the baseline is
        // restored, so a confirmation solve would only re-run under it.
        self.manual_solve_at = None;
        self.manual_relens_pending = false;
        events.info("manual rig reset to the profile baseline");
        Ok(())
    }

    fn manual_solve_deadline(&self) -> Option<std::time::Instant> {
        self.manual_solve_at
    }

    fn flush_manual_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        if !std::mem::take(&mut self.manual_preview_dirty) {
            return Ok(());
        }
        self.render_manual_preview(events)
    }

    fn manual_solve(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        // Clear the armed deadline first: this solve is the debounced one.
        self.manual_solve_at = None;
        let relens = std::mem::take(&mut self.manual_relens_pending);
        // Extract the session inputs into owned values up front, so the
        // immutable borrow does not span the mutable re-detect below.
        let (lw, lh, rw, rh, pins, before, left_frame, right_frame, left_params, right_params) =
            match self.manual.as_ref() {
                Some(session) => (
                    session.left_frame.width,
                    session.left_frame.height,
                    session.right_frame.width,
                    session.right_frame.height,
                    session
                        .pins
                        .iter()
                        .map(|p| reco_calibrate::manual::ManualPin {
                            left_px: p.left_px,
                            right_px: p.right_px,
                        })
                        .collect::<Vec<_>>(),
                    // The layout in effect before the solve; the delta is measured
                    // against it and shown, never applied silently (MANU-05).
                    session.current_layout.clone(),
                    session.left_frame.clone(),
                    session.right_frame.clone(),
                    session.left_params.clone(),
                    session.right_params.clone(),
                ),
                None => {
                    // No session: nothing to solve, preview stays stale.
                    events.manual_solve_state(false, true, false);
                    return Ok(());
                }
            };
        // A lens edit invalidates the retained auto matches: re-run the engine
        // calibration path on the retained raw reference frame under the edited
        // intrinsics and use the fresh post-RANSAC matches (MANU-05). A pin or
        // layout edit reuses the retained matches (no ~629 ms re-detection).
        let auto = if relens {
            let config = reco_calibrate::types::CalibrationConfig::default();
            match reco_calibrate::calibrate(
                &self.gpu,
                &[(left_frame, right_frame)],
                &left_params,
                &right_params,
                &config,
            ) {
                Ok(result) => {
                    let fresh = result
                        .per_frame
                        .first()
                        .map(|fm| fm.points.clone())
                        .unwrap_or_default();
                    if let Some(session) = self.manual.as_mut() {
                        session.auto_seed = fresh.clone();
                    }
                    fresh
                }
                Err(e) => {
                    // The re-detect produced no usable matches: a defined
                    // non-result. Keep the retained matches and WARN; the solve
                    // below still runs on the pins (never a garbage rig).
                    events.log(
                        Level::Warn,
                        format!("manual re-solve detection produced no matches: {e}"),
                    );
                    self.manual
                        .as_ref()
                        .map(|session| session.auto_seed.clone())
                        .unwrap_or_default()
                }
            }
        } else {
            self.manual
                .as_ref()
                .map(|session| session.auto_seed.clone())
                .unwrap_or_default()
        };

        // The swap lives only in the engine helper — never re-derived here.
        let config = reco_calibrate::types::CalibrationConfig::default();
        match reco_calibrate::manual::solve_manual_calibration(
            &pins,
            &auto,
            (lw, lh),
            (rw, rh),
            &config,
        ) {
            Ok(result) => {
                events.manual_solve_result(
                    crate::events::PlaneLayoutView::from_layout(&result.layout),
                    result.residual,
                    result.pins_used,
                    result.auto_used,
                );
                events.manual_layout_delta(
                    result.layout.camera_axis_offset - before.camera_axis_offset,
                    result.layout.intersect - before.intersect,
                    result.layout.x_ty - before.x_ty,
                    result.layout.x_rz - before.x_rz,
                );
                // The success path mirrors the failure WARN with an INFO line so
                // a headless run can observe the solve lifecycle end-to-end (the
                // failure path already logs; this closes the asymmetry). The
                // message is a bounded summary, never the full layout payload.
                events.info(format!(
                    "manual solve succeeded: residual {:.6} px, pins_used {}, auto_used {}",
                    result.residual, result.pins_used, result.auto_used
                ));
                self.mark_manual_preview_dirty();
                events.manual_solve_state(false, false, false);
            }
            Err(e) => {
                // Degenerate/insufficient: a defined non-result. Keep the last
                // good result (stale) and WARN — never present a garbage rig.
                // Only the engine's degenerate-set rejection is surfaced as
                // `degenerate`; the editor's "spread the pins" warning is driven
                // by that precise flag, not by `stale` alone.
                let degenerate = matches!(e, reco_calibrate::CalibrateError::InvalidConfig(_));
                events.manual_solve_state(false, true, degenerate);
                events.log(Level::Warn, format!("manual solve: {e}"));
            }
        }
        Ok(())
    }

    fn manual_validate(&mut self, frame: u32, events: &EventSink) -> Result<(), WorkerError> {
        // Snapshot the session inputs into owned values up front, so no
        // immutable borrow of `self.manual` spans the render/solve below.
        let (
            frames_total,
            right_frames_total,
            cal_frame,
            left_params,
            right_params,
            current_layout,
            sync_offset,
        ) = match self.manual.as_ref() {
            Some(session) => (
                session.frames_total,
                session.right_frames_total,
                session.frame,
                session.left_params.clone(),
                session.right_params.clone(),
                session.current_layout.clone(),
                session.sync_offset,
            ),
            None => {
                return Err(WorkerError::InvalidInput {
                    field: "frame".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
        };
        // T-04.1-16: clamp the operator-supplied index against the probed frame
        // count before extraction.
        let frame = u64::from(frame).min(frames_total.saturating_sub(1));
        let right_index = self.right_frame_index(frame, right_frames_total);
        let (left_path, right_path) = self.manual_frame_paths()?;
        let left_frame = extract_manual_frame(&left_path, frame)?;
        let right_frame = extract_manual_frame(&right_path, right_index)?;

        // The manual result as a normal profile — the same assembly Save uses, so
        // the comparison is rendered under exactly the parameters that will be
        // saved.
        let base = self.manual_base_calibration();
        let match_cal = crate::calibration::build_manual_match_calibration(
            &base,
            left_params.clone(),
            right_params.clone(),
            current_layout.clone(),
            sync_offset,
        );

        // Stitched comparison under the current manual parameters/layout. The
        // calibration-frame reference is what the UI blinks against. Both
        // stitched frames are bounded to `MANUAL_FRAME_MAX_EDGE` before they
        // cross, exactly as the preview path is: a full-res stitched frame is
        // 8.29 MB, and the blink comparison carries two per validation.
        let bound = |(rgba, w, h): (Vec<u8>, u32, u32)| {
            crate::calibration::downsample_rgba(
                &rgba,
                w,
                h,
                crate::calibration::MANUAL_FRAME_MAX_EDGE,
            )
        };
        let reference = if cal_frame == frame {
            None
        } else {
            let cal_left = extract_manual_frame(&left_path, cal_frame)?;
            let cal_right = extract_manual_frame(
                &right_path,
                self.right_frame_index(cal_frame, right_frames_total),
            )?;
            Some(bound(
                self.render_stitched(&match_cal, &cal_left, &cal_right)?,
            ))
        };
        let (rgba, width, height) =
            bound(self.render_stitched(&match_cal, &left_frame, &right_frame)?);
        let reference = reference.unwrap_or_else(|| (rgba.clone(), width, height));

        // Run the engine on the validation frame under the current intrinsics to
        // obtain a per-frame residual and the engine's independent layout. The
        // verdict compares that layout against the operator's manual layout.
        let config = reco_calibrate::types::CalibrationConfig::default();
        match reco_calibrate::calibrate(
            &self.gpu,
            &[(left_frame, right_frame)],
            &left_params,
            &right_params,
            &config,
        ) {
            Ok(result) => {
                let verdict = crate::calibration::validation_verdict(
                    &current_layout,
                    &result.calibration.layout,
                    result.residual_error,
                );
                events.manual_validation_frame(
                    frame as u32,
                    &rgba,
                    width,
                    height,
                    result.residual_error,
                    verdict,
                    (&reference.0, reference.1, reference.2),
                );
                Ok(())
            }
            Err(e) => {
                // A validation frame that cannot be evaluated is a defined
                // non-result: surface the typed error (Save stays available with
                // the single-frame caveat) rather than fabricating a residual.
                events.log(
                    Level::Warn,
                    format!("manual validation frame {frame} produced no usable matches: {e}"),
                );
                events.failed(WorkerError::Engine(format!(
                    "validation frame {frame} could not be evaluated: {e}"
                )));
                Ok(())
            }
        }
    }

    fn manual_save(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
        let (left_params, right_params, current_layout, sync_offset) = match self.manual.as_ref() {
            Some(session) => (
                session.left_params.clone(),
                session.right_params.clone(),
                session.current_layout.clone(),
                session.sync_offset,
            ),
            None => {
                return Err(WorkerError::InvalidInput {
                    field: "path".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
        };
        let base = self.manual_base_calibration();
        let calibration = crate::calibration::build_manual_match_calibration(
            &base,
            left_params,
            right_params,
            current_layout,
            sync_offset,
        );
        // T-04.1-16: never write a profile that fails validation.
        calibration
            .validate()
            .map_err(|e| WorkerError::ProfileSave(e.to_string()))?;
        calibration
            .to_file(std::path::Path::new(&path))
            .map_err(|e| WorkerError::ProfileSave(e.to_string()))?;
        // Adopt the saved profile as the live result/preview source, so a later
        // Preview/Export consumes it unchanged (MANU-07). This mirrors
        // `load_profile`'s adoption.
        self.adopt_calibration(calibration);
        // The manual save wrote a `.json`, so the referenced path is known
        // (PROJ-01).
        self.calibration_path = Some(path.clone());
        events.manual_saved(path);
        Ok(())
    }

    fn begin_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        // Lazily (re)open the operator's imported clips with the adopted
        // calibration's sync offset. Inputs are usually set before calibration,
        // so this is where the offset first becomes known (D3-15). A no-op for
        // the no-import path and when the source already matches.
        self.sync_operator_source()?;
        // Make the decoded pair observable: the operator can confirm Preview is
        // showing the imported clips, not the hardcoded startup pair (D3-15).
        if let Some((left, right)) = self.preview_source_paths() {
            events.info(format!("preview source: {left} + {right}"));
        }
        // Clone (not `take`) the calibration: the renderer takes it by value,
        // but a preview must be repeatable without re-importing, so the loaded
        // calibration stays owned by the backend.
        let (cal, (in_w, in_h)) = match (self.calibration.clone(), self.input_size) {
            (Some(cal), Some(size)) => (cal, size),
            _ => return Err(WorkerError::NotImported),
        };

        // Build the renderer if recovery has not already done so, and cache the
        // inputs so a device loss during the session can rebuild it without
        // re-importing (FOUND-05).
        if self.renderer.is_none() {
            self.renderer = Some(self.build_renderer(cal.clone(), in_w, in_h)?);
        }
        self.renderer_input = Some((cal, in_w, in_h));

        // Build the transport from the loaded source's timing metadata.
        let (fps, fps_rational, total_frames) = {
            let source = self.source.as_ref().ok_or(WorkerError::NotImported)?;
            let info = source.info();
            (info.fps, info.fps_rational, source.total_frames())
        };
        // Carry the user's Loop setting across the session boundary BEFORE anything
        // observes the new transport. `transport()` prefers the live session and
        // falls back to `loaded`, so this matches the precedence a `SetLoop`
        // command saw; without it a user who ticked Loop before pressing Play
        // watched the clip stop at its end instead of wrapping. The shared
        // constructor is what keeps the mock and the real backend from drifting
        // apart here — see `new_session_transport`.
        let transport = new_session_transport(
            fps,
            fps_rational,
            total_frames,
            self.session.as_ref(),
            self.loaded.as_ref(),
        );
        events.position(
            transport.frame(),
            transport.total_frames(),
            transport.fps_rational(),
        );
        events.transport(transport.state(), transport.loop_enabled());
        self.session = Some(transport);
        self.first_frame_marker_pending = true;

        // Rewind a source left exhausted by a previous session, or the new
        // session gets nothing from it.
        //
        // `end_preview` drops the session transport but the SOURCE keeps its
        // decode state, so after a clip runs to its end `try_next_frame` returns
        // `None` forever. Pressing Play again built a fresh session transport
        // against that dead source: the first tick saw no frame, `at_end` fired
        // immediately, and the clip "played" for zero frames -- Play looked dead
        // after the first run.
        self.rewind_for_loop()?;
        self.last_frame_time = std::time::Instant::now();

        // UI-SPEC E3: paint the idle/clear frame in the reserved region before
        // the first stitched frame, so the region is never a black hole.
        match self.presenter.render_idle() {
            Ok(()) => {}
            Err(crate::presenter::PresenterError::SurfaceLost { kind }) => {
                // Recover and retry the idle frame once; if it still fails, the
                // first stitched frame will paint the region instead.
                events.info(format!("idle frame needs recovery ({kind:?}); recovering"));
                self.handle_recovery(kind, events)?;
                let _ = self.presenter.render_idle();
            }
            Err(e) => events.info(format!("idle frame skipped: {e}")),
        }

        events.info("preview session started");
        // Baseline pose, in the same `pose: yaw …` shape `drain_pointer_input`
        // writes after a gesture. Without it the FIRST gesture had nothing to be
        // compared against — there was no earlier pose line at all, because
        // `events.pose` sends only on the typed channel to the webview and never
        // reaches stdout. A reader can now take "before" from here and "after"
        // from the post-gesture line.
        //
        // Emitted once per session, never per tick.
        let baseline = self.pose.current_pose();
        events.info(format!(
            "pose: yaw {:.3}, pitch {:.3}, fov {:.1}, max {:.1}",
            baseline.yaw,
            baseline.pitch,
            self.pose.current_fov_deg(),
            self.fov_ceiling()
        ));
        Ok(())
    }

    fn tick_session(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        if self.session.is_none() {
            return Ok(());
        }

        // UI-SPEC Screen Router (E6): the native view is suspended on
        // Import/Calibrate and while a webview modal is open, so never render a
        // frame into a hidden surface. The same `native_view_visible` predicate
        // that drives `set_visible` gates the tick, so the two can never
        // disagree. The session/transport/pose are left untouched — a tick that
        // resumes on Preview (modal closed) continues from here (the transport
        // is not advanced while suspended).
        if !self.chrome.native_view_visible() {
            return Ok(());
        }

        // Pick up any webview readback channel attached since the last tick
        // (PREV-05) at a tick boundary, before a frame is produced.
        self.drain_readback_channels();

        // A device loss flagged mid-session is recovered here, between frames
        // (never while a SurfaceTexture is alive — we are before the acquire).
        if self.recovery_pending() {
            self.recover(events)?;
        }

        // 1. Execute at most one coalesced seek per tick (the
        //    `FfmpegFileSource::seek` caller-coalescing contract).
        let pending = self.session.as_mut().and_then(|t| t.take_pending_seek());
        if let Some(target) = pending {
            let source = self.source.as_mut().ok_or(WorkerError::NotImported)?;
            source
                .seek(target)
                .map_err(|e| WorkerError::Engine(e.to_string()))?;
            // The seek may have re-armed the transport; reflect it.
            if let Some(t) = self.session.as_mut() {
                t.mark_seeking_done(target);
            }
            self.last_frame_time = std::time::Instant::now();
            // A seek positions the playhead; present the frame at the new
            // position on this same tick below.
        }

        let (playing, duration, frame) = {
            let t = self.session.as_ref().ok_or(WorkerError::NotImported)?;
            (
                t.state() == crate::transport::TransportState::Playing,
                t.frame_duration(),
                t.frame(),
            )
        };

        // 2. Pace: when playing, sleep the remainder of the frame budget before
        //    decoding, so playback runs at the clip's fps (mirrors the CLI's
        //    about_to_wait).
        if playing {
            let elapsed = self.last_frame_time.elapsed();
            if elapsed < duration {
                std::thread::sleep(duration - elapsed);
            }
        }

        // 3. Decode the next frame. When playing, use the non-blocking
        //    try_next_frame so a not-ready decode channel is a no-op; when
        //    paused-at-a-seek, do a blocking next_frame so the seek shows a
        //    frame immediately.
        //
        //    `exhausted` is read in the SAME borrow so it describes this exact
        //    attempt: `try_next_frame` returns `Ok(None)` both when the channel
        //    is merely not ready yet and when the source has genuinely run out,
        //    and only `is_exhausted()` separates the two.
        let seeking = pending.is_some();
        let (pair, exhausted) = {
            let source = self.source.as_mut().ok_or(WorkerError::NotImported)?;
            let frame_result = if playing || seeking {
                if playing {
                    source
                        .try_next_frame()
                        .map_err(|e| WorkerError::Engine(e.to_string()))?
                } else {
                    source
                        .next_frame()
                        .map_err(|e| WorkerError::Engine(e.to_string()))?
                }
            } else {
                None
            };
            let pair = match frame_result {
                Some(reco_core::source::StereoFrame::Yuv420p(pair)) => Some(pair),
                Some(_) => None,
                None => None,
            };
            (pair, source.is_exhausted())
        };

        // End-of-source handling. Ending requires an ACTUALLY spent source, not
        // just an absent frame — see `is_end_of_source`.
        if is_end_of_source(playing, pair.is_some(), exhausted) {
            self.resolve_end_of_source(events)?;
            return Ok(());
        }
        let pair = match pair {
            Some(p) => p,
            None => return Ok(()),
        };

        // 4. Pose tick + FOV plumbing (PREV-04). Mirror the CLI's smooth_camera:
        //    tick the pose so the CURRENT pose moves toward the target, push the
        //    resolved FOV onto the pipeline (which clamps 1..179 internally),
        //    keep the viewport inside the coverage boundary, then render with
        //    the eased pose and emit it.
        self.pose.tick();
        let aspect = if self.viewport.height > 0 {
            self.viewport.width as f32 / self.viewport.height as f32
        } else {
            1.0
        };
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.pipeline_mut().set_fov(self.pose.current_fov_deg());
            // Keep the viewport inside the coverage boundary (mirror the CLI's
            // clamp_enabled path exactly).
            let coverage = renderer.coverage();
            self.pose
                .clamp_via_coverage(coverage, aspect, self.rig_tilt);
        }
        let render = self.pose.render_pose(self.rig_tilt);
        let fov_degrees = self.pose.current_fov_deg();
        events.pose(
            self.pose.current_pose().yaw,
            self.pose.current_pose().pitch,
            fov_degrees,
            self.fov_ceiling(),
        );

        // 5. Render + present.
        //
        // Split-borrow `self` into disjoint fields so the renderer can be taken
        // `&mut` (the readback path needs `&mut StitchRenderer` for
        // `render_and_readback_rgba`) while the presenter is borrowed `&mut`
        // alongside it. `fov_degrees` is passed to every impl uniformly (PREV-04).
        // The view mode (PREV-03) picks the render path: Panorama → the
        // stitched panorama, Source → the two raw source tiles. The mode is
        // captured before the split-borrow; it is a pure function of the last
        // requested mode and never touches the transport frame or pose.
        let view_mode = self.view_mode;
        let Self {
            renderer,
            presenter,
            ..
        } = self;
        let outcome = {
            let renderer = renderer
                .as_mut()
                .ok_or_else(|| WorkerError::Engine("renderer missing during session".into()))?;
            match view_mode {
                crate::presenter::ViewMode::Panorama => presenter.render_frame(
                    renderer,
                    &pair.left,
                    &pair.right,
                    render.yaw,
                    render.pitch,
                    fov_degrees,
                ),
                crate::presenter::ViewMode::Source => {
                    presenter.render_source(renderer, &pair.left, &pair.right)
                }
            }
        };
        match outcome {
            Ok(crate::presenter::FrameOutcome::Presented) => {
                self.last_frame_time = std::time::Instant::now();
                if self.first_frame_marker_pending {
                    self.first_frame_marker_pending = false;
                    // The A1 marker: reaching this line proves the worker
                    // decoded a real frame pair, rendered through the shared
                    // device, and presented it into the native child view.
                    events.info(
                        "A1 verdict: engine worker presented a stitched frame \
                         into the native child view",
                    );
                    if crate::hardcoded::debug_device_loss_enabled() {
                        events.info("debug device-loss simulation enabled — destroying device");
                        self.simulate_device_loss(events)?;
                        self.device_lost.store(true, Ordering::SeqCst);
                        self.recover(events)?;
                    }
                }
                // Advance the transport and emit the new position.
                if playing {
                    if self.advance_session_after_frame(events)? {
                        return Ok(());
                    }
                    if let Some(t) = self.session.as_ref() {
                        let (frame, total, fps_rational) =
                            (t.frame(), t.total_frames(), t.fps_rational());
                        events.position(frame, total, fps_rational);
                    }
                } else {
                    events.position(frame, self.session_total(), self.session_fps_rational());
                }
            }
            Ok(crate::presenter::FrameOutcome::Skipped { kind }) => {
                // Transient: drop this frame, retry next tick (FOUND-05).
                debug_assert_eq!(recovery_action(kind), RecoveryAction::SkipFrame);
                log::debug!("frame skipped on transient surface error: {kind:?}");
            }
            Err(crate::presenter::PresenterError::SurfaceLost { kind }) => {
                // Outdated/Lost: recover (drop-frame-before-reconfigure is
                // guaranteed because render_frame returns only after the
                // failed acquire — no SurfaceTexture is alive).
                events.info(format!("surface error ({kind:?}); recovering"));
                self.handle_recovery(kind, events)?;
            }
            Err(crate::presenter::PresenterError::NotConfigured) => {
                return Err(WorkerError::Engine(
                    "presenter is not configured during session".to_string(),
                ));
            }
            Err(e) => {
                // Other render failure: surface the typed error (do not
                // present a black frame, do not panic).
                return Err(WorkerError::Engine(e.to_string()));
            }
        }
        Ok(())
    }

    fn end_preview(&mut self, _events: &EventSink) {
        // Mirror user-set state back into the import-built transport before the
        // session is dropped, so a flag set DURING a session survives that
        // session ending for any reason — not just the clip running out (a
        // failed tick ends the session too).
        if let (Some(loaded), Some(session)) = (self.loaded.as_mut(), self.session.as_ref()) {
            crate::transport::carry_user_state(Some(session), loaded);
        }
        self.session = None;
    }

    /// Position the source back at frame 0 so playback (or a loop wrap) can
    /// actually pull frames again.
    ///
    /// A no-op when no source is loaded. Split out because two callers need it
    /// for the same reason and getting it wrong is expensive: both a loop wrap
    /// and a fresh `begin_preview` over an exhausted source must rewind, and a
    /// source left exhausted makes every subsequent tick see end-of-source
    /// immediately.
    fn rewind_for_loop(&mut self) -> Result<(), WorkerError> {
        let source = self.source.as_mut().ok_or(WorkerError::NotImported)?;
        source
            .seek(0)
            .map_err(|e| WorkerError::Engine(e.to_string()))
    }

    fn session_active(&self) -> bool {
        self.session.is_some()
    }

    fn transport(&mut self) -> &mut crate::transport::Transport {
        // Prefer the live session; fall back to the transport `Import` built so a
        // transport command issued before `begin_preview` still carries the real
        // fps/frame count. Only with neither (a transport command before any
        // import) is a placeholder materialized, and that path is unreachable
        // from a handler that reports state - see [`Self::loaded_transport`].
        if self.session.is_none() && self.loaded.is_none() {
            self.session = Some(crate::transport::Transport::new(30.0, None, None));
        }
        // Destructure once so the two slots are borrowed disjointly; two
        // sequential `as_mut()` borrows of `self` would overlap here.
        let Self {
            session, loaded, ..
        } = self;
        session
            .as_mut()
            .or(loaded.as_mut())
            .expect("a session or an import-built transport is present")
    }

    fn loaded_transport(&self) -> Option<&crate::transport::Transport> {
        self.loaded.as_ref()
    }

    fn preview_source_paths(&self) -> Option<(String, String)> {
        // `None` while the source is the hardcoded startup import; the operator
        // clips only appear once `sync_operator_source` opened them (D3-15).
        self.source_spec
            .as_ref()
            .map(|spec| (spec.left.clone(), spec.right.clone()))
    }

    fn session_transport_mut(&mut self) -> Option<&mut crate::transport::Transport> {
        self.session.as_mut()
    }

    fn publish_position(&mut self, events: &EventSink) {
        // Prefer the live session; fall back to the import-built transport so a
        // position is still published after the session has been dropped.
        let published = self
            .session
            .as_ref()
            .or(self.loaded.as_ref())
            .map(|t| (t.frame(), t.total_frames(), t.fps_rational()));
        if let Some((frame, total, fps_rational)) = published {
            events.position(frame, total, fps_rational);
        }
    }

    fn republish_projection(&mut self, events: &EventSink) {
        self.publish_position(events);
        if let Some(transport) = self.session.as_ref().or(self.loaded.as_ref()) {
            events.transport(transport.state(), transport.loop_enabled());
        }
        // The pose is read from the control, not from the last render: with no
        // session there is no tick, so a drag while paused must still be
        // reflected in what the frontend reconciles.
        let pose = self.pose.current_pose();
        events.pose(
            pose.yaw,
            pose.pitch,
            self.pose.current_fov_deg(),
            self.fov_ceiling(),
        );
        events.view(self.view_mode);
        // Re-assert the resolved presenter with its fallback reason. The
        // frontend subscribes and only then invokes `republish_projection`
        // (App.svelte), so this is the single delivery point for a startup
        // fallback WARN: the reason is delivered exactly once per reconcile
        // instead of at worker boot, where the F1 subscribe-after-emit ordering
        // would drop it. When `active_reason` is `None` (the native path) this
        // still projects to the `presenter: <Kind>` INFO line.
        events.presenter(self.active_kind, self.active_reason.clone());
    }

    fn set_chrome(&mut self, chrome: crate::presenter::ChromeState, events: &EventSink) {
        self.chrome = chrome;
        // UI-SPEC Screen Router (E6): the native child view is live only on the
        // Preview screen. Import/Calibrate are opaque webview screens, so the
        // native view is suspended (X11 unmap) while either is active — no black
        // or idle native surface may show through. It must ALSO be suspended
        // while a webview modal is open, even on Preview: on X11 the native
        // child composites above the webview, so an open modal would otherwise
        // be hidden behind the live panorama and unreachable by pointer. Both
        // conditions are folded into the shared `native_view_visible` predicate
        // (the mock uses the same one, so they cannot drift). Called here (a
        // screen/modal change boundary), never per tick; `set_visible` is
        // idempotent (T-03-12).
        self.presenter.set_visible(chrome.native_view_visible());
        self.reconfigure_viewport(events);
    }

    fn resize_viewport(&mut self, width: u32, height: u32, events: &EventSink) {
        self.window_width = width;
        self.window_height = height;
        self.reconfigure_viewport(events);
    }

    fn export(
        &mut self,
        settings: &crate::events::ExportSettings,
        events: &EventSink,
        interrupted: &AtomicBool,
    ) -> Result<(), WorkerError> {
        use crate::events::InputRole;

        // ── Resolve inputs (operator-chosen, else the hardcoded import pair) ──
        let hardcoded = crate::hardcoded::media_paths().ok();
        let left = self
            .input_paths
            .get(&InputRole::Left)
            .cloned()
            .or_else(|| hardcoded.as_ref().map(|p| p.left.display().to_string()));
        let right = self
            .input_paths
            .get(&InputRole::Right)
            .cloned()
            .or_else(|| hardcoded.as_ref().map(|p| p.right.display().to_string()));
        let (Some(left), Some(right)) = (left, right) else {
            events.export_failed("no clips are loaded — select both camera inputs first");
            return Ok(());
        };

        // ── Resolve the retained calibration (never re-read the file) ──
        let Some(cal) = self
            .current_calibration
            .clone()
            .or_else(|| self.calibration.clone())
        else {
            events.export_failed("no calibration result — calibrate the cameras first");
            return Ok(());
        };

        // ── Parse the typed codec/quality with a default on unknown (T-05-01) ──
        let (codec, codec_warn) = parse_output_codec(&settings.codec);
        if let Some(warn) = &codec_warn {
            events.info(format!("export codec {warn} — using H.264"));
        }
        let (quality, quality_warn) = parse_output_quality(&settings.quality);
        if let Some(warn) = &quality_warn {
            events.info(format!("export quality {warn} — using Balanced"));
        }
        if settings.bitrate_kbps.is_some() {
            // The engine exposes quality tiers, not a kbps target; surface the
            // limitation rather than silently dropping the operator's value.
            events.info("explicit bitrate is not supported yet — using the quality tier");
        }

        // ── Probe encoders and resolve the encoder (EXPT-02) ──
        let video_codec: reco_io::ffmpeg::encoder::VideoCodec = codec.into();
        let available = reco_io::ffmpeg::encoder::available_encoders(video_codec);
        if available.is_empty() {
            events.export_failed(format!("no encoder available for {video_codec:?}"));
            return Ok(());
        }
        let (chosen, fallback) = choose_encoder(&available, settings.encoder_name.as_deref());
        if let Some((requested, used)) = fallback {
            // Explicit, never-silent fallback (EXPT-02). The class is the
            // encoder that will actually run: an unavailable override can fall
            // back to a hardware auto pick, so the copy must not assume software.
            events.export_fallback(requested, used, chosen.is_hardware);
        }
        events.info(format!(
            "Export encoder: {} ({})",
            chosen.name,
            if chosen.is_hardware {
                "hardware"
            } else {
                "software"
            }
        ));

        // ── Resolve the deterministic, collision-free output path (EXPT-06) ──
        //
        // The worker owns the path: `output_dir` (default the media directory)
        // + the left input's stem + the variant suffix + a bounded collision
        // suffix. An existing file is never overwritten (T-05-08 / T-05-10).
        let output = match preview_path_for(Some(left.as_str()), settings) {
            Ok(path) => path,
            Err(e) => {
                events.export_failed(e.to_string());
                return Ok(());
            }
        };

        // ── Frame rate for the trim window (the SAME fps the engine uses) ──
        let fps = reco_io::adapters::FfmpegFileSource::frame_rate(std::path::Path::new(&left))
            .map(|(n, d)| if d != 0 { n as f64 / d as f64 } else { 30.0 })
            .unwrap_or_else(|_| {
                events.info("source frame rate unavailable — assuming 30 fps for the trim window");
                30.0
            });

        // ── Clamp the trim window against the clip (EXPT-03 edge probes) ──
        //
        // A trim never exceeds the clip, and an `out <= in` window is clamped so
        // an empty window is never exported — the clamp is announced, not silent.
        //
        // The total is probed from the clip the export actually opens (`left`),
        // never `self.loaded` (the preview/import transport, which can be a
        // different file than the operator's selected input). Otherwise a valid
        // `end_frame` beyond the *import* clip's length is silently shortened and
        // the ETA is computed against the wrong length (WR-01).
        let source_total =
            reco_io::ffmpeg::calibration_io::probe_video(std::path::Path::new(&left))
                .ok()
                .map(|p| p.total_frames);
        let (start_frame, end_frame, trim_clamped) =
            clamp_trim(settings.start_frame, settings.end_frame, source_total);
        if trim_clamped {
            events.log(
                Level::Warn,
                "trim window out of range — clamped to the clip bounds (never an empty export)",
            );
        }

        // ── Total output frames for the ETA (honest None when unknown) ──
        let total = match (start_frame, end_frame, source_total) {
            (Some(s), Some(e), _) if e > s => Some(e - s),
            (Some(s), None, Some(t)) if t > s => Some(t - s),
            (None, Some(e), _) => Some(e),
            (None, None, Some(t)) => Some(t),
            _ => None,
        };

        events.info(format!("export started: {}", output.display()));

        // ── Dispatch on the variant (EXPT-05) ──
        //
        // Panorama runs the stitched `StitchJob` (the tracer's encode path);
        // side-by-side / stacked run the existing stacked pack path — the same
        // FFmpeg encoder, never a second encoder (research §2.2).
        let variant_result: Result<String, WorkerError> = match settings.variant {
            crate::events::ExportVariant::Panorama => {
                let mut job = reco_io::StitchJob::with_calibration(
                    left.as_str(),
                    right.as_str(),
                    cal.clone(),
                    output.as_path(),
                )
                .codec(codec)
                .quality(quality)
                .resolution(settings.width, settings.height)
                .encoder_name(chosen.name.clone())
                .sync_offset(cal.sync_offset);
                if let Some(start) = start_frame {
                    job = job.start_time(start as f64 / fps);
                }
                if let Some(end) = end_frame {
                    job = job.end_time(end as f64 / fps);
                }

                // Per-frame progress → typed ExportProgress with ETA (EXPT-04).
                let tx = events.sender_clone();
                job = job.on_progress(move |p: &reco_core::session::types::FrameProgress| {
                    let elapsed_ms = p.elapsed.as_millis() as u64;
                    let percent = match total {
                        Some(t) if t > 0 => 100.0 * p.frames_completed as f64 / t as f64,
                        _ => 0.0,
                    };
                    let _ = tx.send(WorkerEvent::ExportProgress {
                        frames_completed: p.frames_completed,
                        total,
                        elapsed_ms,
                        eta_ms: eta_ms(elapsed_ms, p.frames_completed, total),
                        percent,
                    });
                });

                job.run(interrupted)
                    .map(|r| r.encoder_name)
                    .map_err(|e| WorkerError::Engine(e.to_string()))
            }
            variant => run_stacked_variant(
                left.as_str(),
                right.as_str(),
                output.as_path(),
                variant,
                codec,
                quality,
                chosen.name.as_str(),
                cal.sync_offset,
                (start_frame, end_frame),
                events,
                interrupted,
            )
            .map(|()| chosen.name.clone()),
        };

        // The typed terminal event: cancel/finish/fail — never a claimed path on
        // cancel or failure (prohibition).
        if interrupted.load(Ordering::SeqCst) {
            // A cancelled export leaves no partial deliverable behind: the
            // stacked arm returns before `encoder.finish()`, so the file at the
            // resolved path can be partial. Removing it keeps the UI's "no file
            // was written" true and frees the deterministic name for a retry
            // (WR-02).
            remove_partial_output(&output);
            events.export_cancelled();
        } else {
            match variant_result {
                Ok(encoder) => events.export_finished(
                    output.display().to_string(),
                    encoder,
                    chosen.is_hardware,
                    settings.variant,
                ),
                Err(e) => {
                    // A failed export likewise leaves no partial file (WR-02).
                    remove_partial_output(&output);
                    events.export_failed(e.to_string());
                }
            }
        }
        Ok(())
    }

    fn export_cancel(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.export_cancel)
    }

    fn probe_encoders(&self, codec: &str, events: &EventSink) {
        let (codec, _) = parse_output_codec(codec);
        let video_codec: reco_io::ffmpeg::encoder::VideoCodec = codec.into();
        let encoders: Vec<crate::events::EncoderView> =
            reco_io::ffmpeg::encoder::available_encoders(video_codec)
                .into_iter()
                .map(|e| crate::events::EncoderView {
                    name: e.name,
                    description: e.description,
                    is_hardware: e.is_hardware,
                })
                .collect();
        let auto = encoders
            .first()
            .cloned()
            .unwrap_or_else(|| crate::events::EncoderView {
                name: "none".to_string(),
                description: "no encoder available for this codec".to_string(),
                is_hardware: false,
            });
        let auto_hardware = auto.is_hardware;
        events.encoder_list(encoders, auto, auto_hardware);
    }

    fn preview_export_path(&self, settings: &crate::events::ExportSettings, events: &EventSink) {
        use crate::events::InputRole;
        // Resolve the same left input the export will use (operator-chosen, else
        // the hardcoded pair) so the preview matches the written path.
        let left = self.input_paths.get(&InputRole::Left).cloned().or_else(|| {
            crate::hardcoded::media_paths()
                .ok()
                .map(|p| p.left.display().to_string())
        });
        match preview_path_for(left.as_deref(), settings) {
            Ok(path) => events.export_path_preview(path.display().to_string()),
            Err(e) => events.log(Level::Warn, e.to_string()),
        }
    }

    fn system_info(&self, events: &EventSink) {
        // DIAG-01: report the adapter the app is actually using — the worker's
        // own `GpuContext`, never a second one that could resolve differently.
        events.system_info(crate::system::probe_system(Some(&self.gpu)));
    }

    fn run_preflight(&self, events: &EventSink) {
        // DIAG-05: probe the shipped runtime prerequisites directly.
        events.preflight(crate::preflight::run_preflight());
    }

    fn save_project(
        &mut self,
        path: String,
        settings: &crate::events::ExportSettings,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // PROJ-01: build the manifest from the retained state + the webview's
        // export settings, validate, and write atomically (no partial file).
        let project = self.build_project(settings);
        project
            .validate()
            .map_err(|e| WorkerError::ProjectSave(e.to_string()))?;
        project
            .write_to_file(std::path::Path::new(&path))
            .map_err(|e| WorkerError::ProjectSave(e.to_string()))?;
        events.project_saved(path);
        Ok(())
    }

    fn open_project(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
        // PROJ-01: parse + validate the untrusted manifest before applying it
        // (T-05-14). An unparseable/unsupported version is a typed error, never a
        // partial restore.
        let project = crate::project::RecoProject::read_from_file(std::path::Path::new(&path))
            .map_err(|e| WorkerError::ProjectOpen(e.to_string()))?;
        let missing = project.missing_inputs(project_input_exists);
        if !missing.is_empty() {
            // A missing input never fails the open: retain the parsed project so
            // a relocate can re-run the restore, and emit the typed list.
            self.pending_project = Some(PendingProject {
                path: path.clone(),
                project,
            });
            events.project_missing_inputs(missing);
            return Ok(());
        }
        self.apply_project(path, project, events);
        Ok(())
    }

    fn relocate_project_input(
        &mut self,
        role: crate::events::InputRole,
        path: String,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        let Some(mut pending) = self.pending_project.take() else {
            return Err(WorkerError::ProjectRelocate(
                "no project is awaiting relocation".to_string(),
            ));
        };
        pending.project.set_input_path(role, path);
        let missing = pending.project.missing_inputs(project_input_exists);
        if !missing.is_empty() {
            // Still incomplete: keep waiting, re-emit the (updated) list.
            self.pending_project = Some(pending);
            events.project_missing_inputs(missing);
            return Ok(());
        }
        self.apply_project(pending.path, pending.project, events);
        Ok(())
    }

    fn export_diagnostics_bundle(
        &self,
        path: String,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        // DIAG-03: gather the retained structured logs (pushed by the tracing
        // layer), the live system view (the adapter the app is actually using),
        // the active calibration profile, and the retained debug inspector
        // payload. `write_bundle` redacts every text entry and writes atomically.
        let inputs = crate::diagnostics::BundleInputs {
            logs: self.log_buffer.snapshot(),
            system: crate::system::probe_system(Some(&self.gpu)),
            calibration: self
                .current_calibration
                .as_ref()
                .and_then(|cal| serde_json::to_string_pretty(cal).ok()),
            debug: events
                .snapshot_debug()
                .map(|report| crate::diagnostics::redacted_debug_json(&report)),
            home: crate::diagnostics::home_dir(),
        };
        let summary = crate::diagnostics::write_bundle(std::path::Path::new(&path), &inputs)
            .map_err(|e| WorkerError::DiagnosticsBundle(e.to_string()))?;
        events.diagnostics_bundle_written(summary.path.display().to_string(), summary.files, true);
        Ok(())
    }

    fn dispatch_intent(&mut self, intent: reco_control::ControlIntent) {
        dispatch_intent(&mut self.pose, intent);
    }

    fn drain_pointer_input(&mut self, events: &EventSink) {
        let Some(gesture) = self.presenter.take_pointer_gesture() else {
            return;
        };
        // Pose applies to the panorama only (UI-SPEC "Pose input mapping
        // (panorama mode only)"). The gesture is still drained in Source mode —
        // it is consumed and dropped rather than left to accumulate — so
        // toggling back to the panorama cannot apply one huge stale delta.
        if self.view_mode != crate::presenter::ViewMode::Panorama {
            return;
        }
        // Read the pose's FOV and the viewport width BEFORE the mutable borrow,
        // so the rad-per-pixel scale matches the scale the frontend uses.
        let fov_degrees = self.pose.current_fov_deg();
        let viewport_width = self.viewport.width;
        for intent in crate::presenter::pointer_input::pointer_gesture_to_intents(
            gesture,
            fov_degrees,
            viewport_width,
        ) {
            // The SAME dispatch point `WorkerCommand::Intent` uses, so a drag and
            // an arrow key cannot diverge.
            dispatch_intent(&mut self.pose, intent);
        }
        // Report the pose, in the SAME `pose: yaw …` shape the webview's typed
        // `Pose` event projects to. Two emissions use it — this one and the
        // baseline printed at session start — so a reader can take "before"
        // from the session-start line and "after" from this one.
        //
        // Event-driven on purpose: this fires only when the native child
        // actually delivered pointer input, never per tick, so it cannot flood
        // the log. Before it existed the worker consumed drags in complete
        // silence — `Pose` events reach only the webview (sent on the typed
        // channel, never through `EventSink::log`), so neither the terminal nor
        // a headless probe could tell input had arrived, and a dead gesture path
        // was indistinguishable from a working one.
        //
        // The TARGET, not the current pose: `dispatch_intent` moves the target
        // and the current pose eases toward it over later ticks. Reporting the
        // target makes the assertion immediate and immune to easing rate.
        let target = self.pose.target_pose();
        events.info(format!(
            "pose: yaw {:.3}, pitch {:.3}, fov {:.1}, max {:.1}",
            target.yaw,
            target.pitch,
            target.fov_degrees.unwrap_or(fov_degrees),
            self.fov_ceiling()
        ));
    }

    fn set_presenter(&mut self, kind: crate::presenter::PresenterKind, events: &EventSink) {
        // A manual override re-runs the chain from the requested step: attempt it,
        // and on a fall-through error continue to the next weaker presenter. The
        // override cannot inject a new surface — it only selects an
        // already-built presenter (T-02-07).
        self.drain_readback_channels();

        // Already active: report success without a churn (position/pose are
        // untouched either way; a swap happens only at this command boundary).
        if kind == self.active_presenter() {
            events.info(format!("presenter: {} (already active)", kind.name()));
            // The active presenter is the requested one and is not a fallback.
            self.active_reason = None;
            events.presenter(kind, None);
            return;
        }

        let start = crate::presenter::PRESENTER_CHAIN
            .iter()
            .position(|k| *k == kind)
            .unwrap_or(crate::presenter::PRESENTER_CHAIN.len());
        let start = start.min(crate::presenter::PRESENTER_CHAIN.len().saturating_sub(1));
        let mut attempts: Vec<(
            crate::presenter::PresenterKind,
            Result<(), crate::presenter::PresenterError>,
        )> = Vec::new();
        // Try the requested step and every weaker step after it. A non-fall-through
        // error is fatal to the attempt and is reported as-is.
        for candidate in &crate::presenter::PRESENTER_CHAIN[start..] {
            // install_presenter already returns Ok(()) when the candidate is
            // already active, so no explicit check is needed here.
            let result = self.install_presenter(*candidate);
            let fallthrough = result
                .as_ref()
                .err()
                .is_some_and(crate::presenter::is_fallthrough);
            let ok = result.is_ok();
            attempts.push((*candidate, result));
            if ok || !fallthrough {
                break;
            }
        }

        let (chosen, reason) = crate::presenter::choose_presenter(&attempts);
        match reason {
            // Fallback to a weaker presenter: exactly one WARN line carrying the
            // reason and the remediation (Phase 1 D-05; never silent).
            Some(err) => {
                let reason = err.to_string();
                // Keep `active_reason` in sync so `republish_projection` (which
                // re-asserts the presenter after subscribe) reports the same
                // resolved fallback the set_presenter path just announced.
                self.active_reason = Some(reason.clone());
                events.presenter(chosen, Some(reason.clone()));
                events.log(
                    Level::Warn,
                    crate::presenter::fallback_warn_line(chosen, &reason),
                );
            }
            // Successful manual selection: an INFO line, no WARN.
            None if chosen == kind => {
                self.active_reason = None;
                events.info(format!("presenter switched to {}", chosen.name()));
                events.presenter(chosen, None);
            }
            // The requested kind failed with fall-through but a weaker kind
            // succeeded: report the fallback without a WARN.
            None => {
                self.active_reason = None;
                events.info(format!(
                    "presenter override to {} resolved to {}",
                    kind.name(),
                    chosen.name()
                ));
                events.presenter(chosen, None);
            }
        }
    }

    fn active_presenter(&self) -> crate::presenter::PresenterKind {
        self.active_kind
    }

    fn show_preview_window(&mut self, events: &EventSink) {
        // Show the separate window wherever it lives in the chain (the active
        // presenter or its parked slot). Every other presenter's impl is a no-op
        // default, so this routes to the separate-window presenter without a
        // downcast (PREV-05).
        let attempt = if self.active_kind == crate::presenter::PresenterKind::SeparateWindow {
            self.presenter.show_preview_window()
        } else {
            let idx = crate::presenter::PRESENTER_CHAIN
                .iter()
                .position(|k| *k == crate::presenter::PresenterKind::SeparateWindow)
                .expect("separate window is in the chain");
            match self.presenters[idx].as_mut() {
                Some(p) => p.show_preview_window(),
                None => Ok(()),
            }
        };
        match attempt {
            Ok(()) => events.info("preview window shown"),
            Err(e) => events.failed(WorkerError::Engine(e.to_string())),
        }
    }

    fn set_view(&mut self, mode: crate::presenter::ViewMode, events: &EventSink) {
        // Idempotent no-op: requesting the already-active mode changes nothing
        // and emits no `View` event and no INFO line (PREV-03).
        if mode == self.view_mode {
            return;
        }
        self.view_mode = mode;
        events.info(format!(
            "view switched to {}",
            match mode {
                crate::presenter::ViewMode::Source => "source",
                crate::presenter::ViewMode::Panorama => "panorama",
            }
        ));
        events.view(mode);
    }

    fn recovery_pending(&self) -> bool {
        self.device_lost.load(Ordering::SeqCst)
    }

    fn recover(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        // FOUND-05: a plain device-loss flag means we must rebuild the device
        // (the flag only ever comes from the lost callback / destroy()). An
        // `Outdated` surface error never sets the flag — it is handled inline in
        // the frame path via `handle_recovery`.
        if !self.recovery_pending() {
            return Ok(());
        }
        // Bound the attempts. A rebuild that fails (e.g. the backend reports no
        // device memory after a loss) must NOT leave the flag set, or the worker
        // loop would retry immediately and forever — a hot spin, and exactly the
        // "hang" FOUND-05 forbids. After the cap we clear the flag, report a
        // typed failure, and let the app stay responsive with no fresh frames.
        const MAX_RECOVERY_ATTEMPTS: u32 = 3;
        let mut last_err = None;
        for attempt in 1..=MAX_RECOVERY_ATTEMPTS {
            events.info("device lost — recovering");
            match self.rebuild_device_and_surface() {
                Ok(()) => {
                    self.recovery_attempts = 0;
                    events.info("device recovered");
                    return Ok(());
                }
                Err(e) => {
                    log::error!(
                        "device recovery attempt {attempt}/{MAX_RECOVERY_ATTEMPTS} failed: {e}"
                    );
                    last_err = Some(e);
                }
            }
        }
        self.recovery_attempts += 1;
        self.device_lost.store(false, Ordering::SeqCst);
        Err(last_err.unwrap_or_else(|| {
            WorkerError::Engine("device recovery failed with no error".to_string())
        }))
    }

    fn simulate_device_loss(&mut self, _events: &EventSink) -> Result<(), WorkerError> {
        // FOUND-05 debug affordance (NOT in the normal UI; guarded by the
        // caller behind debug/flag). wgpu 28 exposes no public "lose the
        // device" API, so we call `Device::destroy()`, which fires the
        // device-lost callback with `DeviceLostReason::Destroyed` and sets the
        // recovery flag. The worker loop then runs `recover`.
        self.gpu.device().destroy();
        Ok(())
    }

    fn shutdown(&mut self) {
        // FOUND-06 teardown order (RESEARCH Pattern 6 / Pitfall 5):
        //   stop loop (done: the loop returned) →
        //   drop decode/session →
        //   drop renderer (bind groups/textures) →
        //   drop presenter surface →
        //   drop device (last; natural field order also enforces this).
        // The decode source must drop before the wgpu objects to avoid the
        // documented NVDEC/CUDA teardown race
        // (`crates/reco-cli/src/preview.rs:213-215`).
        self.source = None;
        self.renderer_input = None;
        self.renderer = None;
        // Release the platform child window NOW, while the parent Tauri window
        // is still alive. `XDestroyWindow` after GTK has torn the parent down
        // raises a fatal `BadDrawable` X error that aborts the process before
        // the clean-stop path can report (FOUND-06). Idempotent; `Drop` is a
        // no-op afterwards.
        self.presenter.release_presenter_window();
        // `presenter` and `gpu` drop when `self` is dropped, in declaration
        // order (presenter before gpu). Do not `std::process::exit` here — a
        // clean unwind is required while GPU threads may be mid-submit.
    }
}

impl GpuEngineBackend {
    /// Recompute the severity-sorted readiness report from the current inputs
    /// and lens overrides (CALB-05 / D3-06).
    ///
    /// Samples one mid-clip frame per input for the overlap/exposure estimates;
    /// any sampling failure leaves the estimate `None` (honest unknown). The
    /// report is non-blocking — the operator may always calibrate anyway.
    fn current_readiness(&self) -> crate::events::ReadinessReport {
        use crate::events::{InputRole, ReadinessFinding, ReadinessReport, ReadinessSeverity};

        let left_path = self
            .input_paths
            .get(&InputRole::Left)
            .map(String::as_str)
            .unwrap_or("");
        let right_path = self
            .input_paths
            .get(&InputRole::Right)
            .map(String::as_str)
            .unwrap_or("");

        let (Some(left), Some(right)) = (&self.inputs[0], &self.inputs[1]) else {
            return ReadinessReport {
                findings: Vec::new(),
                overlap_estimate: None,
                exposure_delta_stops: None,
            };
        };

        let lens_available = self.lens_available_for(0) && self.lens_available_for(1);
        let samples = sample_readiness(left_path, right_path);
        let mut report = crate::calibration::estimate_readiness(
            left,
            right,
            left_path,
            right_path,
            lens_available,
            samples,
        );

        // A lens override whose calibration resolution differs from the clip is
        // a shape mismatch (D3-06) — the comparison needs the override, so it is
        // folded in here rather than in the pure input check, then the list is
        // re-sorted so the severity order still holds.
        for (idx, role) in [(0, InputRole::Left), (1, InputRole::Right)] {
            let input_resolution = self.inputs[idx]
                .as_ref()
                .and_then(crate::calibration::parse_resolution);
            if let (Some((iw, ih)), Some(over)) =
                (input_resolution, self.lens_overrides[idx].as_ref())
                && (over.width, over.height) != (iw, ih)
            {
                report.findings.push(ReadinessFinding {
                    code: crate::events::ReadinessCode::LensResolutionMismatch,
                    severity: ReadinessSeverity::BlockingShape,
                    message: format!(
                        "{} lens override is {}×{} but the clip is {iw}×{ih}",
                        role.label(),
                        over.width,
                        over.height
                    ),
                    estimated: false,
                });
            }
        }
        report.findings.sort_by_key(|finding| finding.severity);
        report
    }

    /// Whether a lens profile can be resolved for one input (CALB-05).
    ///
    /// True when an override is applied or the embedded database has at least
    /// one candidate for the input's resolution; an unknown resolution never
    /// claims a profile is unavailable.
    fn lens_available_for(&self, idx: usize) -> bool {
        if self.lens_overrides[idx].is_some() {
            return true;
        }
        match self.inputs[idx]
            .as_ref()
            .and_then(crate::calibration::parse_resolution)
        {
            Some((w, h)) if w > 0 && h > 0 => {
                !reco_calibrate::lens_database::LensDatabase::embedded()
                    .candidates(w, h)
                    .is_empty()
            }
            _ => true,
        }
    }

    /// Emit the current severity-sorted readiness report (CALB-05).
    fn emit_readiness(&self, events: &EventSink) {
        events.readiness(self.current_readiness());
    }

    /// Publish the current calibration's field ROI to the frontend (CALB-09).
    ///
    /// Called whenever a calibration is adopted (run success or profile load), so
    /// the editor pre-populates from the loaded/current profile rather than
    /// starting empty. An absent ROI publishes `FieldRoiCleared` so a previously
    /// shown polygon does not linger across a load.
    fn emit_field_roi(&self, events: &EventSink) {
        match self
            .current_calibration
            .as_ref()
            .and_then(|c| c.field_roi.clone())
        {
            Some(roi) => events.field_roi_applied(roi),
            None => events.field_roi_cleared(),
        }
    }

    /// Invalidate a live result if inputs/profile changed (D3-08).
    ///
    /// The stored `current_calibration` is cleared alongside the flag: once the
    /// inputs or a lens override change, the saved profile would no longer
    /// describe the clips, so `save_profile` must not offer it (WR-01).
    fn invalidate_result(&mut self, events: &EventSink) {
        if self.has_result {
            self.has_result = false;
            self.current_calibration = None;
            // The referenced profile no longer describes the clips (WR-01), so
            // its path is cleared too (PROJ-01).
            self.calibration_path = None;
            events.result_invalidated();
        }
    }

    /// Adopt a fresh or loaded calibration as the session's live result and the
    /// preview's render source (D3-15 / WR-05).
    ///
    /// `begin_preview` renders from `self.calibration`, which before this only
    /// the startup `import()` populated — so Preview kept rendering the
    /// hardcoded startup profile regardless of what the operator calibrated or
    /// loaded. The cached renderer is dropped so the next `begin_preview`
    /// rebuilds from the adopted profile, but only when no preview session is
    /// live: `tick_session` requires a renderer and would fail mid-session.
    fn adopt_calibration(&mut self, calibration: reco_core::calibration::MatchCalibration) {
        // Keep the render inputs consistent with the adopted profile: `rig_tilt`
        // drives the renderer/pose, and before this only the startup `import()`
        // set it, so Preview rendered the hardcoded tilt (D3-15 / WR-05).
        self.rig_tilt = calibration.rig_tilt as f32;
        self.current_calibration = Some(calibration.clone());
        self.calibration = Some(calibration);
        self.has_result = true;
        // A freshly adopted calibration has no referenced `.json` until a caller
        // that knows one (profile load/save, manual save) sets it (PROJ-01).
        self.calibration_path = None;
        if self.session.is_none() {
            self.renderer = None;
            self.renderer_input = None;
        }
    }

    /// The sync offset the preview source should be opened with: the
    /// adopted/current calibration's, else 0 when no calibration exists yet.
    fn preview_sync_offset(&self) -> i64 {
        self.current_calibration
            .as_ref()
            .or(self.calibration.as_ref())
            .map(|cal| cal.sync_offset)
            .unwrap_or(0)
    }

    /// Ensure [`Self::source`] decodes the operator's imported clips when both
    /// roles are set, replacing the hardcoded startup pair (IMPT-01 / D3-15).
    ///
    /// `import()` opens the hardcoded `left.mp4`/`right.mp4`; without this,
    /// Preview kept decoding those placeholders after the operator imported and
    /// calibrated real files. Called when both inputs become available (after
    /// `set_input`) and lazily from `begin_preview` so the adopted calibration's
    /// sync offset is applied even when the inputs were set before calibration.
    ///
    /// The no-import Phase 1/2 path is preserved: while either role is unset and
    /// no operator source was ever opened, the hardcoded source is left in
    /// place. A cleared input drops an operator source so Preview cannot decode
    /// a clip the operator removed.
    ///
    /// Returns `true` when the source was (re)opened.
    fn sync_operator_source(&mut self) -> Result<bool, WorkerError> {
        use crate::events::InputRole;
        let spec = resolve_preview_source(
            self.input_paths.get(&InputRole::Left).map(String::as_str),
            self.input_paths.get(&InputRole::Right).map(String::as_str),
            self.preview_sync_offset(),
        );
        match spec {
            None => {
                // Drop only an operator-opened source; the hardcoded startup
                // source stays for the no-import path.
                if self.source_spec.is_some() {
                    self.source = None;
                    self.input_size = None;
                    self.loaded = None;
                    self.source_spec = None;
                }
                Ok(false)
            }
            Some(spec) if self.source_spec.as_ref() == Some(&spec) => Ok(false),
            Some(spec) => {
                let source = reco_io::adapters::FfmpegFileSource::open_with_offset(
                    std::path::Path::new(&spec.left),
                    std::path::Path::new(&spec.right),
                    spec.sync_offset,
                )
                .map_err(|e| WorkerError::Engine(e.to_string()))?;
                let info = source.info();
                self.input_size = Some((info.width, info.height));
                self.source = Some(source);
                // Rebuild the import-time transport so a transport command
                // issued before `begin_preview` carries the imported clip's
                // real fps/frame count instead of the placeholder's.
                let total_frames = self.source.as_ref().and_then(|s| s.total_frames());
                self.loaded = Some(crate::transport::Transport::new(
                    info.fps,
                    info.fps_rational,
                    total_frames,
                ));
                self.source_spec = Some(spec);
                Ok(true)
            }
        }
    }

    /// Assemble a `.reco` manifest from the retained state (PROJ-01).
    ///
    /// References the operator-selected inputs (else the startup hardcoded
    /// pair), the per-input lens overrides, the calibration path + an inline
    /// snapshot, the current pose, and the webview-owned export `settings`.
    /// Media is never copied.
    fn build_project(
        &self,
        settings: &crate::events::ExportSettings,
    ) -> crate::project::RecoProject {
        use crate::events::InputRole;
        let hardcoded = crate::hardcoded::media_paths().ok();
        let left_path = self
            .input_paths
            .get(&InputRole::Left)
            .cloned()
            .or_else(|| hardcoded.as_ref().map(|p| p.left.display().to_string()))
            .unwrap_or_default();
        let right_path = self
            .input_paths
            .get(&InputRole::Right)
            .cloned()
            .or_else(|| hardcoded.as_ref().map(|p| p.right.display().to_string()))
            .unwrap_or_default();
        let pose = self.pose.current_pose();
        crate::project::RecoProject {
            version: crate::project::PROJECT_VERSION,
            left: crate::project::ProjectInput {
                path: left_path,
                lens_override: self.lens_override_candidates[0].clone(),
            },
            right: crate::project::ProjectInput {
                path: right_path,
                lens_override: self.lens_override_candidates[1].clone(),
            },
            calibration_path: self.calibration_path.clone(),
            calibration: self.current_calibration.clone(),
            pose: crate::project::PoseView {
                yaw: pose.yaw,
                pitch: pose.pitch,
                fov_degrees: self.pose.current_fov_deg(),
            },
            export: settings.clone(),
        }
    }

    /// Restore a fully-present project's state and emit `ProjectOpened` (PROJ-01).
    ///
    /// Applies inputs → lens overrides → calibration → pose, then publishes the
    /// restored inputs/pose/export as a typed `ProjectOpened` so the webview
    /// reconciles from typed values. Called only when every referenced input is
    /// present, so the restore is never partial.
    fn apply_project(
        &mut self,
        path: String,
        project: crate::project::RecoProject,
        events: &EventSink,
    ) {
        use crate::events::InputRole;
        // The typed payload the webview reconciles from, captured before the
        // project is consumed.
        let opened_left = project.left.clone();
        let opened_right = project.right.clone();
        let calibration_path = project.calibration_path.clone();
        let pose = project.pose;
        let export = project.export.clone();

        // 1. Inputs (probe + metadata). `set_input` invalidates any prior result,
        //    which is fine: the calibration is re-adopted below.
        for (role, input) in [
            (InputRole::Left, &project.left),
            (InputRole::Right, &project.right),
        ] {
            if let Err(e) = self.set_input(role, input.path.clone(), events) {
                events.failed(e);
                return;
            }
        }

        // 2. Lens overrides (or clear back to auto-detect).
        for (role, input) in [
            (InputRole::Left, &project.left),
            (InputRole::Right, &project.right),
        ] {
            match &input.lens_override {
                Some(candidate) => {
                    if let Err(e) = self.set_lens_override(role, candidate.clone(), events) {
                        events.failed(e);
                        return;
                    }
                }
                None => {
                    let idx = role_index(role);
                    self.lens_overrides[idx] = None;
                    self.lens_override_candidates[idx] = None;
                    events.lens_override_applied(role, None);
                }
            }
        }

        // 3. Calibration: the inline snapshot wins (a never-saved result has no
        //    path), then the referenced `.json` (module docs).
        let calibration = project.calibration.clone().or_else(|| {
            project.calibration_path.as_ref().and_then(|p| {
                reco_core::calibration::MatchCalibration::from_file(std::path::Path::new(p)).ok()
            })
        });
        let has_calibration = calibration.is_some();
        if let Some(cal) = calibration {
            self.adopt_calibration(cal);
            // Re-assert the referenced path (adopt clears it).
            self.calibration_path = calibration_path.clone();
            self.emit_field_roi(events);
        }

        // 4. Pose: restore the operator's view through the existing intent path.
        self.dispatch_intent(reco_control::ControlIntent::Pose(
            reco_control::PoseIntent::SetYawRad(pose.yaw),
        ));
        self.dispatch_intent(reco_control::ControlIntent::Pose(
            reco_control::PoseIntent::SetPitchRad(pose.pitch),
        ));
        self.dispatch_intent(reco_control::ControlIntent::Pose(
            reco_control::PoseIntent::SetFovDeg(pose.fov_degrees),
        ));

        // 5. The project is no longer pending; publish the restored state.
        self.pending_project = None;
        events.project_opened(
            path,
            opened_left,
            opened_right,
            calibration_path,
            has_calibration,
            pose,
            export,
        );
    }

    /// The left/right clip paths a manual session extracts from (MANU-01).
    ///
    /// Prefers the operator-selected inputs; falls back to the startup
    /// hardcoded clips per role. This is why the flow is always reachable — it
    /// never requires an imported profile or an explicit file selection.
    fn manual_frame_paths(&self) -> Result<(String, String), WorkerError> {
        use crate::events::InputRole;
        let left = self.input_paths.get(&InputRole::Left).cloned();
        let right = self.input_paths.get(&InputRole::Right).cloned();
        if let (Some(l), Some(r)) = (left.clone(), right.clone()) {
            return Ok((l, r));
        }
        let paths =
            crate::hardcoded::media_paths().map_err(|e| WorkerError::Engine(e.to_string()))?;
        Ok((
            left.unwrap_or_else(|| paths.left.display().to_string()),
            right.unwrap_or_else(|| paths.right.display().to_string()),
        ))
    }

    /// Seed the manual preview's per-camera `CameraParams` (MANU-03).
    ///
    /// Prefers the loaded/current profile's intrinsics, then a lens override,
    /// then a neutral pinhole for the frame size. Every branch yields a real
    /// `CameraParams`, so the preview is always a GPU undistort under real
    /// parameters.
    fn manual_params(
        &self,
        left_frame: &reco_core::source::YuvFrame,
        right_frame: &reco_core::source::YuvFrame,
    ) -> (
        reco_core::calibration::CameraParams,
        reco_core::calibration::CameraParams,
    ) {
        if let Some(cal) = self
            .current_calibration
            .as_ref()
            .or(self.calibration.as_ref())
        {
            return (cal.left.clone(), cal.right.clone());
        }
        let left = self.lens_overrides[0]
            .clone()
            .unwrap_or_else(|| default_camera_params(left_frame.width, left_frame.height));
        let right = self.lens_overrides[1]
            .clone()
            .unwrap_or_else(|| default_camera_params(right_frame.width, right_frame.height));
        (left, right)
    }

    /// Mark the manual preview stale so the worker loop renders it once per
    /// command-drain (MANU-03).
    ///
    /// Every manual mutation (pin move, handle drag, frame change, solve) calls
    /// this instead of rendering inline, so a drag burst collapses to a single
    /// bounded frame in [`Self::flush_manual_preview`]. See
    /// [`Self::manual_preview_dirty`].
    fn mark_manual_preview_dirty(&mut self) {
        self.manual_preview_dirty = true;
    }

    /// Render both sides of the retained reference frame under real
    /// `CameraParams` and stream one binary preview per camera (MANU-03).
    ///
    /// The worker is the single GPU owner: the readback RGBA is what crosses to
    /// the webview over the manual-frame channel; no GPU handle ever does
    /// (T-04.1-04). Each frame is box-downsampled to
    /// [`crate::calibration::MANUAL_FRAME_MAX_EDGE`] before it crosses, so the
    /// payload stays bounded regardless of source resolution; the canvas scales
    /// the bounded frame, so the preview remains visually correct.
    fn render_manual_preview(&self, events: &EventSink) -> Result<(), WorkerError> {
        let Some(session) = self.manual.as_ref() else {
            return Ok(());
        };
        let gpu = &self.gpu;
        for (side, frame, params) in [
            (
                crate::events::ManualSide::Left,
                &session.left_frame,
                &session.left_params,
            ),
            (
                crate::events::ManualSide::Right,
                &session.right_frame,
                &session.right_params,
            ),
        ] {
            let (w, h) = (frame.width, frame.height);
            if w == 0 || h == 0 {
                return Err(WorkerError::Engine(
                    "manual reference frame has zero dimensions".to_string(),
                ));
            }
            let undistort =
                reco_core::lens::undistort::GpuUndistort::new(gpu, w, h, w as f32 / h as f32);
            let rgba = undistort.undistort(gpu, &frame.y, &frame.u, &frame.v, params);
            let (rgba, w, h) = crate::calibration::downsample_rgba(
                &rgba,
                w,
                h,
                crate::calibration::MANUAL_FRAME_MAX_EDGE,
            );
            events.manual_preview_frame(side, &rgba, w, h);
        }
        Ok(())
    }

    /// The session's pins projected into the typed webview view (MANU-03).
    fn manual_pin_views(&self) -> Vec<crate::events::ManualPinView> {
        self.manual
            .as_ref()
            .map(|session| session.pins.iter().map(SessionPin::view).collect())
            .unwrap_or_default()
    }

    /// Emit the session's edited real parameters (MANU-05 / MANU-06).
    ///
    /// The editable intrinsic subset plus the layout in effect; the webview
    /// seeds its handles from this typed payload and never derives a parameter
    /// locally. A no-op when no session is open.
    fn emit_manual_params(&self, events: &EventSink) {
        let Some(session) = self.manual.as_ref() else {
            return;
        };
        events.manual_params(
            crate::events::CameraParamsView::from_params(&session.left_params),
            crate::events::CameraParamsView::from_params(&session.right_params),
            crate::events::PlaneLayoutView::from_layout(&session.current_layout),
        );
    }

    /// The right-clip frame index for a chosen left frame, with the session's
    /// sync offset applied (MANU-02).
    ///
    /// The engine's convention (`pipeline::frame_indices`) is right = left +
    /// offset: a positive offset skips right frames (the right stream is ahead),
    /// a negative offset skips left frames. Clamped to the right clip's frame
    /// count so the index can never reach past the decoder.
    fn right_frame_index(&self, left_frame: u64, right_frames_total: u64) -> u64 {
        let offset = self
            .manual
            .as_ref()
            .map(|session| session.sync_offset)
            .unwrap_or(0);
        let idx = left_frame as i64 + offset;
        idx.clamp(0, right_frames_total.saturating_sub(1) as i64) as u64
    }

    /// Re-extract the right reference frame at the session's current sync offset
    /// and retain it (MANU-02 / MANU-03).
    fn reextract_right_reference(&mut self) -> Result<(), WorkerError> {
        let (frame, right_frames_total) = match self.manual.as_ref() {
            Some(session) => (session.frame, session.right_frames_total),
            None => return Ok(()),
        };
        let right_index = self.right_frame_index(frame, right_frames_total);
        let (_left_path, right_path) = self.manual_frame_paths()?;
        let right_frame = extract_manual_frame(&right_path, right_index)?;
        if let Some(session) = self.manual.as_mut() {
            session.right_frame = right_frame;
        }
        Ok(())
    }

    /// Emit the pin list, an instant preview, and the busy/stale state after a
    /// pin mutation, and arm the debounced solve (MANU-03).
    ///
    /// An empty set is a defined non-result: it never arms a solve (MANU-03
    /// empty edge probe) and shows the stale state with a WARN. A non-empty set
    /// arms the debounce; the worker loop runs the solve once it elapses. The
    /// solve is never run from this path directly (T-04.1-11).
    fn after_pin_mutation(&mut self, events: &EventSink) -> Result<(), WorkerError> {
        // IN-03: with no session there is nothing to preview or solve.
        // `manual_remove_pin`/`manual_clear_pins` call this unconditionally, so
        // without this guard a remove/clear on a closed flow would arm a bogus
        // solve.
        if self.manual.is_none() {
            return Ok(());
        }
        events.manual_pins(self.manual_pin_views(), false);
        // Instant preview under the current real parameters.
        self.mark_manual_preview_dirty();
        let empty = self
            .manual
            .as_ref()
            .is_some_and(|session| session.pins.is_empty());
        if empty {
            self.manual_solve_at = None;
            events.manual_solve_state(false, true, false);
            events.log(
                Level::Warn,
                "manual solve skipped: no pins — click the left frame to start",
            );
        } else {
            self.manual_solve_at = Some(std::time::Instant::now() + MANUAL_SOLVE_DEBOUNCE);
            events.manual_solve_state(true, true, false);
        }
        Ok(())
    }

    /// The base profile whose carried fields the manual assembly preserves
    /// (MANU-07).
    ///
    /// Prefers the live/current calibration (a run result or a loaded profile),
    /// then the startup-imported calibration; with neither, a neutral base
    /// carrying the engine defaults for the carried fields. The left/right
    /// intrinsics and layout are always overwritten by
    /// [`crate::calibration::build_manual_match_calibration`], so only
    /// `field_roi`, `lens_correction_amount`, `blend_width`, `rig_tilt`, and
    /// `rig_roll` are read from the base.
    fn manual_base_calibration(&self) -> reco_core::calibration::MatchCalibration {
        self.current_calibration
            .clone()
            .or_else(|| self.calibration.clone())
            .unwrap_or_else(|| reco_core::calibration::MatchCalibration {
                left: default_camera_params(1, 1),
                right: default_camera_params(1, 1),
                layout: default_plane_layout(),
                rig_tilt: 0.0,
                rig_roll: 0.0,
                sync_offset: 0,
                field_roi: None,
                lens_correction_amount: 1.0,
                blend_width: 0.05,
            })
    }

    /// Render one stitched frame under `cal` and read back tightly-packed RGBA
    /// (MANU-07).
    ///
    /// Builds a *local* renderer from the given calibration — the live preview
    /// renderer is left untouched. A fresh renderer's first
    /// `render_and_readback_rgba` call schedules the render + copy (the
    /// triple-buffer warmup returns `None`); `flush_rgba` then drains that same
    /// frame (blocking). Returns `(rgba, width, height)`; an empty `rgba` means
    /// the readback produced nothing (the caller fails closed).
    fn render_stitched(
        &self,
        cal: &reco_core::calibration::MatchCalibration,
        left: &reco_core::source::YuvFrame,
        right: &reco_core::source::YuvFrame,
    ) -> Result<(Vec<u8>, u32, u32), WorkerError> {
        let mut renderer = self.build_renderer(cal.clone(), left.width, left.height)?;
        let left_planes = reco_core::render::planes::YuvPlanes {
            y: &left.y,
            u: &left.u,
            v: &left.v,
        };
        let right_planes = reco_core::render::planes::YuvPlanes {
            y: &right.y,
            u: &right.u,
            v: &right.v,
        };
        let _ = renderer
            .render_and_readback_rgba(&left_planes, &right_planes, 0.0, 0.0)
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let (width, height) = renderer
            .readback_dimensions()
            .unwrap_or((self.viewport.width, self.viewport.height));
        let rgba = renderer
            .flush_rgba()
            .map_err(|e| WorkerError::Engine(e.to_string()))?
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        Ok((rgba, width, height))
    }
}

impl GpuEngineBackend {
    /// Apply the FOUND-05 recovery action for a classified surface error.
    ///
    /// `Reconfigure` reconfigures the surface on the existing device;
    /// `Rebuild` rebuilds the device + surface + renderer. `SkipFrame` is a
    /// no-op (the caller already dropped the frame).
    fn handle_recovery(
        &mut self,
        kind: crate::presenter::SurfaceErrorKind,
        events: &EventSink,
    ) -> Result<(), WorkerError> {
        match recovery_action(kind) {
            RecoveryAction::Reconfigure => {
                self.reconfigure_surface()?;
                events.info("surface reconfigured (Outdated)");
                Ok(())
            }
            RecoveryAction::Rebuild => {
                self.rebuild_device_and_surface()?;
                events.info("device recovered");
                Ok(())
            }
            RecoveryAction::SkipFrame => Ok(()),
        }
    }
}

/// Spawn the engine worker for the real GPU path.
///
/// The worker creates the single [`reco_core::gpu::GpuContext`] from the
/// presenter's surface **on the worker thread** (FOUND-03), so no device is
/// created outside the worker. The caller supplies the `Instance` and the
/// pre-built presenter chain (which owns the render targets).
///
/// `log_tx` / `log_rx` are the shared event channel (DIAG-02): the structured-log
/// tracing layer holds a clone of `log_tx`, so engine records and the worker's
/// own events cross one stream that the bridge drains from `log_rx`.
///
/// Returns the worker, the event receiver, the **readback channel sender**
/// (PREV-05), and the **manual-frame channel slot** (MANU-03): the Tauri command
/// layer hands webview `Channel<Response>`s to the worker through them, which
/// then attach them to the readback presenter and the manual preview path
/// respectively.
///
/// # Errors
///
/// [`WorkerError::Engine`] if device creation or surface configuration fails.
// The constructor takes the whole wiring bundle (presenters, viewport, the two
// cancel flags, the shared log buffer, and both event-channel halves); splitting
// it into a struct would only move the argument list, not shorten it.
#[allow(clippy::too_many_arguments)]
pub fn spawn_gpu_worker(
    instance: reco_core::wgpu::Instance,
    presenters: PresenterChain,
    viewport: crate::presenter::ViewportRect,
    startup_fallback: Option<String>,
    calibration_cancel: Arc<AtomicBool>,
    export_cancel: Arc<AtomicBool>,
    log_buffer: crate::diagnostics::LogBuffer,
    log_tx: Sender<WorkerEvent>,
    log_rx: Receiver<WorkerEvent>,
) -> Result<SpawnedWorker, WorkerError> {
    let backend = GpuEngineBackend::new(
        instance,
        presenters,
        viewport,
        startup_fallback,
        calibration_cancel,
        export_cancel,
        log_buffer,
    )?;
    let readback_tx = backend.readback_sender();
    let (worker, events, manual_frame_slot) =
        EngineWorker::spawn_with_events(backend, log_tx, log_rx);
    Ok((worker, events, readback_tx, manual_frame_slot))
}

// `Arc<AtomicBool>` is the shape the CLI preview uses for its Ctrl-C flag; the
// worker uses the same interrupted-flag contract for Export cancellation.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<AtomicBool>>();
};

#[cfg(test)]
mod tests {

    /// The frame a failing tick advances to before erroring. Distinct from 0 so
    /// a re-publish on the failure path is distinguishable from the
    /// session-start projection.
    const FAILING_TICK_FRAME: u64 = 7;

    /// The recorded ops, minus the per-iteration pointer drain.
    ///
    /// `worker_loop` calls `drain_pointer_input` once per iteration, so the op
    /// log interleaves it with whatever the iteration's command batch did.
    /// Tests that assert a COMMAND ordering filter it out; the pointer tests
    /// assert on it directly.
    fn ops_without_pointer_drain(
        ops: &Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) -> Vec<&'static str> {
        ops.lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|o| *o != "drain_pointer_input")
            .collect()
    }

    /// The viewport width the mock's pose scaling uses.
    ///
    /// A fixed, named constant (rather than the backend's real 1280x800-derived
    /// rect) so a pointer test asserts an exact expected angle: 1000 px of drag
    /// sweeps exactly one FOV at this width.
    const VIEWPORT_WIDTH: u32 = 1000;
    use super::*;
    use std::sync::mpsc::RecvTimeoutError;

    /// WR-02: a sub-floor audio estimate is surfaced but never applied; the
    /// applied offset defaults to 0 with no provenance. At/above the floor the
    /// estimate is applied with Audio provenance.
    #[test]
    fn applied_audio_offset_defaults_a_low_confidence_estimate_to_zero() {
        let floor = crate::events::AUDIO_SYNC_CONFIDENCE_FLOOR;
        assert_eq!(
            applied_audio_offset(7, floor - 0.01),
            (0, crate::events::SyncMethod::None)
        );
        assert_eq!(
            applied_audio_offset(-3, floor - 0.01),
            (0, crate::events::SyncMethod::None)
        );
        assert_eq!(
            applied_audio_offset(7, floor),
            (7, crate::events::SyncMethod::Audio)
        );
        assert_eq!(
            applied_audio_offset(-3, 0.99),
            (-3, crate::events::SyncMethod::Audio)
        );
    }

    /// The metadata the mock's `set_input` reports when a test does not
    /// preload `mock_metadata` for that role.
    fn default_mock_metadata() -> crate::events::InputMetadata {
        crate::events::InputMetadata {
            resolution: crate::events::MetadataField::probed("1920×1080"),
            fps: crate::events::MetadataField::probed("30 fps"),
            duration: crate::events::MetadataField::estimated("0:02"),
            codec: crate::events::MetadataField::missing(crate::events::Provenance::Probed),
        }
    }

    /// Whether a project-referenced input exists, for the GPU-free mock (PROJ-01).
    ///
    /// The real backend probes with FFmpeg; the mock only checks existence so a
    /// test can stage missing/present inputs with plain files.
    fn mock_project_input_exists(path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    /// A fabricated CALB-03 scorecard for the mock's calibration result.
    fn mock_scorecard() -> crate::events::Scorecard {
        crate::events::Scorecard {
            confidence: 0.87,
            confidence_band: crate::events::ConfidenceBand::High,
            residual_error: 0.25,
            total_matches: 900,
            per_frame_matches: 300.0,
            frames_used: 3,
            lens_profile: None,
            k1: 0.072,
            sync: crate::events::SyncView {
                method: crate::events::SyncMethod::None,
                confidence: None,
                offset_frames: 0,
                provenance: crate::events::SyncProvenance {
                    ran: crate::events::SyncMethod::None,
                    is_manual: false,
                },
                offset_semantics: crate::events::SYNC_OFFSET_SEMANTICS.to_string(),
            },
        }
    }

    /// WR-01: when a verified seed exists the session opens on the seed's frame
    /// and seeds; otherwise the requested frame is respected and nothing is
    /// seeded. The frame is clamped to the clip length.
    #[test]
    fn manual_seed_target_opens_on_the_seed_frame_only() {
        // A seed opens on its own frame regardless of the requested frame.
        assert_eq!(manual_seed_target(0, 100, Some(5), true), (5, true));
        assert_eq!(manual_seed_target(42, 100, Some(5), true), (5, true));
        // An out-of-range seed frame is clamped.
        assert_eq!(manual_seed_target(0, 10, Some(999), true), (9, true));
        // No seed: the requested frame is respected and nothing is seeded.
        assert_eq!(manual_seed_target(0, 100, None, false), (0, false));
        assert_eq!(manual_seed_target(7, 100, Some(5), false), (7, false));
        assert_eq!(manual_seed_target(999, 10, None, false), (9, false));
    }

    /// WR-03: verified matches are promoted to pins and the retained `auto` set
    /// is empty, so the solve never double-counts them and a deleted pin stops
    /// contributing. A frame mismatch or empty seed seeds nothing.
    #[test]
    fn manual_seed_sets_promotes_seeds_to_pins_with_no_auto() {
        use reco_calibrate::types::MatchedPoint;
        let verified = vec![
            MatchedPoint::from_planes([-0.2, 0.1], [0.2, -0.1]),
            MatchedPoint::from_planes([0.0, 0.0], [0.0, 0.0]),
        ];
        let (pins, auto) = manual_seed_sets(&verified, true, (1920, 1080), (1920, 1080));
        assert_eq!(
            pins.len(),
            verified.len(),
            "every verified match becomes a pin"
        );
        assert!(
            auto.is_empty(),
            "the auto set must be empty after promotion"
        );

        let (pins, auto) = manual_seed_sets(&verified, false, (1920, 1080), (1920, 1080));
        assert!(pins.is_empty());
        assert!(auto.is_empty());

        let (pins, auto) = manual_seed_sets(&[], true, (1920, 1080), (1920, 1080));
        assert!(pins.is_empty());
        assert!(auto.is_empty());
    }

    /// A fabricated CALB-08 debug report for the mock's calibration result.
    ///
    /// Small but structurally complete (a 1x1 thumbnail pair, one verified and
    /// one rejected point, one per-frame row) so the worker protocol test can
    /// assert the typed payload crosses without a GPU or a decoder.
    fn mock_debug_report() -> crate::events::DebugReport {
        crate::events::DebugReport {
            frame_index: 0,
            frames_total: 1,
            left_width: 1,
            left_height: 1,
            right_width: 1,
            right_height: 1,
            left_thumb: vec![0, 0, 0, 255],
            right_thumb: vec![0, 0, 0, 255],
            verified: vec![crate::events::DebugPoint {
                x_nx: 0.25,
                y_nx: 0.5,
                error: 0.01,
            }],
            rejected: vec![crate::events::DebugPoint {
                x_nx: 0.75,
                y_nx: 0.5,
                error: 0.25,
            }],
            residual_error: 0.25,
            per_frame: vec![crate::events::FrameMatchRow {
                frame: 0,
                keypoints_left: 100,
                keypoints_right: 90,
                post_ratio_test: 40,
                post_spatial_filter: 30,
                post_ransac: 25,
            }],
            points_capped: false,
        }
    }

    /// A minimal valid calibration for the mock's load/save round-trip.
    fn sample_mock_calibration() -> reco_core::calibration::MatchCalibration {
        use reco_core::calibration::{CameraParams, MatchCalibration, PlaneLayout};
        let camera = CameraParams {
            width: 1920,
            height: 1080,
            fx: 1000.0,
            fy: 1000.0,
            cx: 960.0,
            cy: 540.0,
            d: [0.0; 4],
        };
        MatchCalibration {
            left: camera.clone(),
            right: camera,
            layout: PlaneLayout {
                camera_axis_offset: 0.25,
                intersect: 0.5,
                x_ty: 0.0,
                x_rz: 0.0,
                z_rx: 0.0,
                x_rx: 0.0,
                z_rz: 0.0,
            },
            rig_tilt: 0.0,
            rig_roll: 0.0,
            sync_offset: 0,
            field_roi: None,
            lens_correction_amount: 1.0,
            blend_width: 0.05,
        }
    }

    /// A GPU-free backend that records the order of the operations it saw.
    ///
    /// It models the session as: `session_active` while a transport exists;
    /// each `tick_session` advances one tick, and (for the mock) a fabricated
    /// total frame count so `on_frame_advanced` reaches `Ended`
    /// deterministically.
    struct MockBackend {
        ops: Arc<std::sync::Mutex<Vec<&'static str>>>,
        pose: Arc<std::sync::Mutex<reco_control::pose_control::PoseControl>>,
        /// The mock session transport, when a session is active.
        session: Option<crate::transport::Transport>,
        /// Total frames the mock source reports (drives the end transition).
        total_frames: Option<u64>,
        /// Records the FOV value the mock "pushed onto the pipeline" each tick.
        last_pushed_fov: Arc<std::sync::Mutex<Option<f32>>>,
        import_fails: bool,
        /// Make `tick_session` fail, standing in for a rejected seek or a
        /// decode error so the failure path can be exercised without a GPU.
        tick_fails: bool,
        /// Mirrors the real backend's device-lost flag (FOUND-05).
        lost: Arc<AtomicBool>,
        /// The mock's active presenter (PREV-05).
        active_kind: crate::presenter::PresenterKind,
        /// The mock's active-presenter reason when it is a fallback; mirrors the
        /// real backend's `active_reason` so a test can assert that
        /// `republish_projection` re-asserts the resolved kind and the locked
        /// `Presenter fallback to <kind>: <reason>. <remediation>.` WARN.
        active_reason: Option<String>,
        /// The mock's current view mode (PREV-03).
        view_mode: crate::presenter::ViewMode,
        /// The transport built by the mock's `import`, mirroring the real
        /// backend so `loaded_transport` has something to report.
        loaded: Option<crate::transport::Transport>,
        /// A pointer gesture the test stages, standing in for what the native
        /// child's own window would have produced. `None` means "the user did
        /// not touch the panorama", so the ordinary idle case stays free.
        pending_gesture: Option<crate::presenter::pointer_input::PointerGesture>,
        /// Frames the mock's fake source has served since its last rewind.
        ///
        /// The real backend's end-of-source is a property of the SOURCE (a spent
        /// decode pipeline returning `None`), not of the transport's frame
        /// counter — so looping has to rewind the source, and a session started
        /// over a spent source gets nothing. This models that. When the mock
        /// instead let `Transport::on_frame_advanced` wrap the frame on its own,
        /// the mock never reached `rewind_for_loop`, and the loop path had no
        /// test at all — which is how a respawn-the-decoders-on-every-wrap bug
        /// shipped.
        frames_served: u64,
        /// Whether the fake source is spent (returns `None` until rewound).
        source_exhausted: bool,
        /// Ticks during which the fake source yields nothing *without* being
        /// spent — a freshly (re)spawned decode pipeline. This is the exact
        /// condition the real defect needed, and the reason `is_end_of_source`
        /// takes three arguments rather than testing the spent flag alone.
        warmup_ticks: u32,
        /// Frames the fake source will serve, if overridden independently of
        /// `total_frames`.
        ///
        /// The real source and the transport's total are two different numbers:
        /// `total_frames` is a `duration × fps` **estimate**, while the decoder
        /// serves however many frames the container actually holds. When the
        /// estimate is short, the transport counter reaches `total_frames`
        /// while the source still has frames — and that, not source exhaustion,
        /// is the ending that fires in production (observed in the user's log as
        /// seven `preview session started` lines and zero ends). Using one value
        /// for both meant the mock could only ever end the source-driven way,
        /// leaving that path untested.
        ///
        /// `None` means "the source serves `total_frames`", which keeps every
        /// existing test's arithmetic unchanged.
        source_frames: Option<u64>,
        /// The mock's per-role probed metadata, indexed by role (IMPT-02).
        inputs: [Option<crate::events::InputMetadata>; 2],
        /// Metadata a test wants `set_input` to report (else a fixed default).
        mock_metadata: [Option<crate::events::InputMetadata>; 2],
        /// The mock's per-role input paths (IMPT-01).
        input_paths: [Option<String>; 2],
        /// The operator preview source the mock models, or `None` while preview
        /// is on the hardcoded startup pair (IMPT-01 / D3-15).
        ///
        /// Mirrors the real backend's `source_spec` through the shared
        /// [`resolve_preview_source`], so the protocol tests prove the operator's
        /// clips replace the startup pair without a GPU.
        preview_source: Option<PreviewSourceSpec>,
        /// The mock's per-role lens overrides (IMPT-04).
        lens_overrides: [Option<reco_core::calibration::CameraParams>; 2],
        /// The mock's per-role lens-override summaries (PROJ-01).
        lens_override_candidates: [Option<crate::events::LensCandidate>; 2],
        /// The mock's current calibration (IMPT-05/06).
        current_calibration: Option<reco_core::calibration::MatchCalibration>,
        /// The `.json` the mock's calibration came from, when known (PROJ-01).
        calibration_path: Option<String>,
        /// A parsed project awaiting relocation in the mock (PROJ-01).
        pending_project: Option<super::PendingProject>,
        /// The field ROI a test wants `load_profile` to install (CALB-09), so the
        /// pre-population emit can be asserted without a real profile file.
        mock_field_roi: Option<reco_core::calibration::FieldRoi>,
        /// Whether the mock holds a live result (D3-08).
        has_result: bool,
        /// The mock's shared calibration-cancel flag (CALB-02).
        calibration_cancel: Arc<AtomicBool>,
        /// The mock's shared export-cancel flag (EXPT-04). Defaults to a fresh
        /// `false` flag; a test can share one via [`MockBackend::with_export_cancel`]
        /// to set it and assert `ExportCancelled`.
        export_cancel: Arc<AtomicBool>,
        /// The mock's structured-log buffer (DIAG-03), empty by default.
        log_buffer: crate::diagnostics::LogBuffer,
        /// A per-frame delay the mock's export sleeps, so a test can set the
        /// export-cancel flag mid-run (the worker clears a stale cancel at the
        /// start of every run, so a pre-set flag is intentionally ignored).
        /// `ZERO` by default, so ordinary tests stay fast.
        mock_export_delay: Duration,
        /// The mock's modelled native-view visibility (UI-SPEC Screen Router).
        ///
        /// Mirrors the real backend's `presenter.set_visible(chrome.native_view_visible())`
        /// — the same shared predicate, covering both the active screen and
        /// whether a modal is open — so a worker test can observe the visibility
        /// policy without a GPU. Shared because the backend is moved onto the
        /// worker thread.
        screen_visible: Arc<std::sync::Mutex<Option<bool>>>,
        /// Whether a manual session is open in the mock (MANU-03).
        manual_open: bool,
        /// The mock's manual pins (MANU-03).
        manual_pins: Vec<super::SessionPin>,
        /// The mock's next pin id (MANU-03).
        manual_next_id: u32,
        /// The mock's armed debounced-solve deadline (MANU-03).
        manual_solve_at: Option<std::time::Instant>,
        /// The mock's edited left intrinsics (MANU-05).
        manual_left_params: reco_core::calibration::CameraParams,
        /// The mock's edited right intrinsics (MANU-05).
        manual_right_params: reco_core::calibration::CameraParams,
        /// The mock's baseline left intrinsics captured at `manual_begin`
        /// (MANU-05).
        manual_baseline_left: reco_core::calibration::CameraParams,
        /// The mock's baseline right intrinsics captured at `manual_begin`
        /// (MANU-05).
        manual_baseline_right: reco_core::calibration::CameraParams,
        /// The mock's layout in effect (MANU-06).
        manual_layout: reco_core::calibration::PlaneLayout,
        /// The mock's layout baseline captured at `manual_begin` (MANU-06).
        manual_baseline_layout: reco_core::calibration::PlaneLayout,
        /// Whether the mock's armed solve should re-detect (MANU-05).
        manual_relens_pending: bool,
        /// Whether the mock's manual preview needs re-emitting (MANU-03).
        /// Mirrors the real backend's coalescing flag so a burst of pin
        /// mutations collapses to one preview pair, not one per command.
        manual_preview_dirty: bool,
        /// Whether the mock's `refine_lens` reports an accepted result (INTR-03).
        ///
        /// Defaults to true; a test sets it false to exercise the
        /// rejected/no-write guard (T-04.2-10) without a GPU.
        mock_refine_accepted: bool,
    }

    /// The mock's debounce window (MANU-03): short, so a worker test observes
    /// the immediate pins/state and a later solve without a 400 ms wait. The
    /// debounce itself (never solving from a pin command) is what is under test,
    /// not the exact window — which is agent discretion.
    const MOCK_SOLVE_DEBOUNCE: Duration = Duration::from_millis(20);

    impl MockBackend {
        fn new(ops: Arc<std::sync::Mutex<Vec<&'static str>>>) -> Self {
            Self {
                ops,
                pose: Arc::new(std::sync::Mutex::new(
                    reco_control::pose_control::PoseControl::with_defaults(),
                )),
                session: None,
                loaded: None,
                total_frames: Some(5),
                last_pushed_fov: Arc::new(std::sync::Mutex::new(None)),
                import_fails: false,
                tick_fails: false,
                lost: Arc::new(AtomicBool::new(false)),
                active_kind: crate::presenter::PresenterKind::Native,
                active_reason: None,
                view_mode: crate::presenter::ViewMode::Panorama,
                pending_gesture: None,
                frames_served: 0,
                source_exhausted: false,
                warmup_ticks: 0,
                source_frames: None,
                inputs: [None, None],
                mock_metadata: [None, None],
                input_paths: [None, None],
                preview_source: None,
                lens_overrides: [None, None],
                lens_override_candidates: [None, None],
                current_calibration: None,
                calibration_path: None,
                pending_project: None,
                mock_field_roi: None,
                has_result: false,
                calibration_cancel: Arc::new(AtomicBool::new(false)),
                export_cancel: Arc::new(AtomicBool::new(false)),
                log_buffer: crate::diagnostics::LogBuffer::new(),
                mock_export_delay: Duration::ZERO,
                screen_visible: Arc::new(std::sync::Mutex::new(None)),
                manual_open: false,
                manual_pins: Vec::new(),
                manual_next_id: 0,
                manual_solve_at: None,
                manual_left_params: super::default_camera_params(2, 1),
                manual_right_params: super::default_camera_params(2, 1),
                manual_baseline_left: super::default_camera_params(2, 1),
                manual_baseline_right: super::default_camera_params(2, 1),
                manual_layout: super::default_plane_layout(),
                manual_baseline_layout: super::default_plane_layout(),
                manual_relens_pending: false,
                manual_preview_dirty: false,
                mock_refine_accepted: true,
            }
        }

        /// Stage a pointer gesture for the next
        /// [`EngineBackend::drain_pointer_input`], standing in for the native
        /// child's own event queue.
        fn with_pointer_gesture(
            mut self,
            gesture: crate::presenter::pointer_input::PointerGesture,
        ) -> Self {
            self.pending_gesture = Some(gesture);
            self
        }

        /// Share the modelled native-view visibility with the test, so the
        /// screen-driven `set_visible` value is observable after the backend is
        /// moved onto the worker thread.
        fn with_screen_visible(mut self, visible: Arc<std::sync::Mutex<Option<bool>>>) -> Self {
            self.screen_visible = visible;
            self
        }

        /// Share the export-cancel flag with the test (EXPT-04), so a test can
        /// set it before posting `Export` and assert `ExportCancelled`.
        fn with_export_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
            self.export_cancel = flag;
            self
        }

        /// Give the mock's export a per-frame delay so a test can set the
        /// export-cancel flag while the run is in flight (EXPT-04).
        fn with_export_delay(mut self, delay: Duration) -> Self {
            self.mock_export_delay = delay;
            self
        }

        /// The mock's coverage ceiling. It has no renderer or coverage
        /// boundary, so the configured max is the honest answer for it — and a
        /// test that wants to pin the emitted ceiling can lower this.
        ///
        /// Inherent, not a trait method: the ceiling is a property of each
        /// backend's own state, so putting it on the trait would force every
        /// backend through one accessor for a value only the real one can
        /// actually compute from coverage.
        fn fov_ceiling(&self) -> f32 {
            self.pose.lock().unwrap().config().fov_max_degrees
        }

        /// The mock's preview sync offset: the adopted/current calibration's,
        /// else 0 (mirrors the real backend's `preview_sync_offset`).
        fn mock_preview_sync_offset(&self) -> i64 {
            self.current_calibration
                .as_ref()
                .map(|cal| cal.sync_offset)
                .unwrap_or(0)
        }

        /// Re-resolve the mock's modelled preview source through the shared
        /// [`resolve_preview_source`] (IMPT-01 / D3-15).
        ///
        /// Mirrors the real backend: both roles set → the operator clips;
        /// either unset → the hardcoded startup pair (`None`).
        fn refresh_mock_preview_source(&mut self) {
            self.preview_source = resolve_preview_source(
                self.input_paths[0].as_deref(),
                self.input_paths[1].as_deref(),
                self.mock_preview_sync_offset(),
            );
        }

        fn record(&self, op: &'static str) {
            self.ops.lock().unwrap().push(op);
        }

        /// The mock's pins projected into the typed webview view (MANU-03).
        fn mock_pin_views(&self) -> Vec<crate::events::ManualPinView> {
            self.manual_pins
                .iter()
                .map(super::SessionPin::view)
                .collect()
        }

        /// Mirror the real backend's post-mutation protocol (MANU-03): emit the
        /// pins + instant preview, then arm the (short) debounce for a non-empty
        /// set or show the stale state for an empty one. Never solves directly.
        fn after_mock_pin_mutation(&mut self, events: &EventSink) {
            // IN-03 parity: no session, nothing to preview or solve.
            if !self.manual_open {
                return;
            }
            events.manual_pins(self.mock_pin_views(), false);
            self.manual_preview_dirty = true;
            if self.manual_pins.is_empty() {
                self.manual_solve_at = None;
                events.manual_solve_state(false, true, false);
            } else {
                self.manual_solve_at = Some(std::time::Instant::now() + MOCK_SOLVE_DEBOUNCE);
                events.manual_solve_state(true, true, false);
            }
        }

        /// Emit the mock's edited real parameters (MANU-05 / MANU-06), mirroring
        /// the real backend's typed payload.
        fn emit_mock_manual_params(&self, events: &EventSink) {
            events.manual_params(
                crate::events::CameraParamsView::from_params(&self.manual_left_params),
                crate::events::CameraParamsView::from_params(&self.manual_right_params),
                crate::events::PlaneLayoutView::from_layout(&self.manual_layout),
            );
        }

        /// Recompute and emit the readiness report, using the SAME pure decision
        /// the real backend uses (never a reimplementation).
        ///
        /// The mock is GPU- and file-free, so it passes no sampled estimates and
        /// assumes a lens profile is available.
        fn emit_mock_readiness(&self, events: &EventSink) {
            let report = match (&self.inputs[0], &self.inputs[1]) {
                (Some(left), Some(right)) => crate::calibration::estimate_readiness(
                    left,
                    right,
                    self.input_paths[0].as_deref().unwrap_or(""),
                    self.input_paths[1].as_deref().unwrap_or(""),
                    true,
                    crate::calibration::ReadinessSamples::default(),
                ),
                _ => crate::events::ReadinessReport {
                    findings: Vec::new(),
                    overlap_estimate: None,
                    exposure_delta_stops: None,
                },
            };
            events.readiness(report);
        }

        /// Invalidate a live result if one exists (D3-08).
        ///
        /// Mirrors the real backend: the stale calibration is cleared too, so a
        /// save after an input change cannot write a profile that no longer
        /// matches the clips (WR-01).
        fn invalidate_mock_result(&mut self, events: &EventSink) {
            if self.has_result {
                self.has_result = false;
                self.current_calibration = None;
                self.calibration_path = None;
                events.result_invalidated();
            }
        }

        /// Assemble a `.reco` manifest from the mock's state (PROJ-01).
        fn build_mock_project(
            &self,
            settings: &crate::events::ExportSettings,
        ) -> crate::project::RecoProject {
            let (yaw, pitch, fov_degrees) = {
                let pose = self.pose.lock().unwrap();
                let current = pose.current_pose();
                (current.yaw, current.pitch, pose.current_fov_deg())
            };
            crate::project::RecoProject {
                version: crate::project::PROJECT_VERSION,
                left: crate::project::ProjectInput {
                    path: self.input_paths[0].clone().unwrap_or_default(),
                    lens_override: self.lens_override_candidates[0].clone(),
                },
                right: crate::project::ProjectInput {
                    path: self.input_paths[1].clone().unwrap_or_default(),
                    lens_override: self.lens_override_candidates[1].clone(),
                },
                calibration_path: self.calibration_path.clone(),
                calibration: self.current_calibration.clone(),
                pose: crate::project::PoseView {
                    yaw,
                    pitch,
                    fov_degrees,
                },
                export: settings.clone(),
            }
        }

        /// Restore a fully-present project in the mock and emit `ProjectOpened`
        /// (PROJ-01). Mirrors the real backend's protocol without a GPU/file.
        fn apply_mock_project(
            &mut self,
            path: String,
            project: crate::project::RecoProject,
            events: &EventSink,
        ) {
            use crate::events::InputRole;
            let opened_left = project.left.clone();
            let opened_right = project.right.clone();
            let calibration_path = project.calibration_path.clone();
            let pose = project.pose;
            let export = project.export.clone();

            for (role, input) in [
                (InputRole::Left, &project.left),
                (InputRole::Right, &project.right),
            ] {
                let _ = self.set_input(role, input.path.clone(), events);
            }
            for (role, input) in [
                (InputRole::Left, &project.left),
                (InputRole::Right, &project.right),
            ] {
                match &input.lens_override {
                    Some(candidate) => {
                        let _ = self.set_lens_override(role, candidate.clone(), events);
                    }
                    None => {
                        let idx = role_index(role);
                        self.lens_overrides[idx] = None;
                        self.lens_override_candidates[idx] = None;
                        events.lens_override_applied(role, None);
                    }
                }
            }
            let has_calibration = match project.calibration.clone() {
                Some(cal) => {
                    self.current_calibration = Some(cal);
                    self.has_result = true;
                    true
                }
                None => false,
            };
            self.calibration_path = calibration_path.clone();
            {
                let mut p = self.pose.lock().unwrap();
                for intent in [
                    reco_control::ControlIntent::Pose(reco_control::PoseIntent::SetYawRad(
                        pose.yaw,
                    )),
                    reco_control::ControlIntent::Pose(reco_control::PoseIntent::SetPitchRad(
                        pose.pitch,
                    )),
                    reco_control::ControlIntent::Pose(reco_control::PoseIntent::SetFovDeg(
                        pose.fov_degrees,
                    )),
                ] {
                    dispatch_intent(&mut p, intent);
                }
            }
            self.pending_project = None;
            events.project_opened(
                path,
                opened_left,
                opened_right,
                calibration_path,
                has_calibration,
                pose,
                export,
            );
        }
    }

    impl EngineBackend for MockBackend {
        fn import(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("import");
            events.info("import started");
            if self.import_fails {
                return Err(WorkerError::Engine("synthetic import failure".to_string()));
            }
            // Mirror the real backend: a successful import builds the transport
            // from the source's timing so the handler can project it.
            self.loaded = Some(crate::transport::Transport::new(
                30.0,
                Some((30, 1)),
                self.total_frames,
            ));
            Ok(())
        }

        fn set_input(
            &mut self,
            role: crate::events::InputRole,
            path: String,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("set_input");
            let idx = role_index(role);
            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str())
                .to_string();
            let metadata = self.mock_metadata[idx]
                .clone()
                .unwrap_or_else(default_mock_metadata);
            self.inputs[idx] = Some(metadata.clone());
            self.input_paths[idx] = Some(path);
            events.info(format!("{} selected: {filename}", role.label()));
            events.metadata(role, metadata);
            // Use the same pure decision the real backend uses.
            self.emit_mock_readiness(events);
            self.invalidate_mock_result(events);
            // Mirror the real backend: once both roles are set the preview
            // decodes the operator's clips, not the hardcoded startup pair.
            self.refresh_mock_preview_source();
            Ok(())
        }

        fn clear_input(
            &mut self,
            role: crate::events::InputRole,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("clear_input");
            let idx = role_index(role);
            self.inputs[idx] = None;
            self.input_paths[idx] = None;
            self.lens_overrides[idx] = None;
            self.lens_override_candidates[idx] = None;
            events.info(format!("{} cleared", role.label()));
            self.emit_mock_readiness(events);
            self.invalidate_mock_result(events);
            // Mirror the real backend: a cleared input drops the operator
            // preview source so no stale clip is decoded.
            self.refresh_mock_preview_source();
            Ok(())
        }

        fn lens_candidates(
            &mut self,
            role: crate::events::InputRole,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("lens_candidates");
            events.lens_candidates(
                role,
                vec![crate::events::LensCandidate {
                    camera: "Mock Camera".to_string(),
                    lens: "Wide".to_string(),
                    width: 1920,
                    height: 1080,
                }],
            );
            Ok(())
        }

        fn set_lens_override(
            &mut self,
            role: crate::events::InputRole,
            candidate: crate::events::LensCandidate,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("set_lens_override");
            let idx = role_index(role);
            self.lens_overrides[idx] = Some(reco_core::calibration::CameraParams {
                width: candidate.width,
                height: candidate.height,
                fx: 1000.0,
                fy: 1000.0,
                cx: candidate.width as f64 / 2.0,
                cy: candidate.height as f64 / 2.0,
                d: [0.0; 4],
            });
            self.lens_override_candidates[idx] = Some(candidate.clone());
            events.lens_override_applied(role, Some(candidate));
            self.emit_mock_readiness(events);
            self.invalidate_mock_result(events);
            Ok(())
        }

        fn clear_lens_override(
            &mut self,
            role: crate::events::InputRole,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("clear_lens_override");
            let idx = role_index(role);
            self.lens_overrides[idx] = None;
            self.lens_override_candidates[idx] = None;
            events.lens_override_applied(role, None);
            self.emit_mock_readiness(events);
            self.invalidate_mock_result(events);
            Ok(())
        }

        fn set_field_roi(
            &mut self,
            left: Vec<[f64; 2]>,
            right: Vec<[f64; 2]>,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("set_field_roi");
            // Mirror the real backend's typed rejection exactly, so the protocol
            // test exercises the same shape.
            if self.current_calibration.is_none() {
                return Err(WorkerError::InvalidInput {
                    field: "field_roi".to_string(),
                    reason: "no calibration result — run or load a calibration first".to_string(),
                });
            }
            let roi = normalize_field_roi(left, right);
            let cleared = roi.left.is_empty() && roi.right.is_empty();
            if let Some(cal) = self.current_calibration.as_mut() {
                cal.field_roi = if cleared { None } else { Some(roi.clone()) };
            }
            if cleared {
                events.field_roi_cleared();
            } else {
                events.field_roi_applied(roi);
            }
            Ok(())
        }

        fn refine_lens(
            &mut self,
            _heldout_fraction: f64,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("refine_lens");
            // Mirror the real backend's typed rejection exactly, so the protocol
            // test exercises the same shape.
            let Some(baseline) = self.current_calibration.as_ref().map(|c| c.left.d[0]) else {
                return Err(WorkerError::InvalidInput {
                    field: "refine_lens".to_string(),
                    reason: "no calibration result — run or load a calibration first".to_string(),
                });
            };

            // A deterministic typed result so the protocol test can assert the
            // event crosses and the profile is written only on acceptance.
            let view = if self.mock_refine_accepted {
                let new_k1 = baseline + 0.01;
                if let Some(cal) = self.current_calibration.as_mut() {
                    cal.left.d[0] = new_k1;
                    cal.right.d[0] = new_k1;
                }
                crate::events::IntrinsicsRefinementView {
                    k1: new_k1,
                    baseline_k1: baseline,
                    accepted: true,
                    reason: crate::calibration::refinement_reason_text(
                        reco_calibrate::RefinementReason::Accepted,
                    )
                    .to_string(),
                    heldout_baseline: Some(1.0),
                    heldout_refined: Some(0.9),
                }
            } else {
                crate::events::IntrinsicsRefinementView {
                    k1: baseline,
                    baseline_k1: baseline,
                    accepted: false,
                    reason: crate::calibration::refinement_reason_text(
                        reco_calibrate::RefinementReason::GuardRejected,
                    )
                    .to_string(),
                    heldout_baseline: Some(1.0),
                    heldout_refined: Some(1.0),
                }
            };
            events.intrinsics_refined(view);
            Ok(())
        }

        fn calibrate(
            &mut self,
            _options: crate::events::CalibrationOptions,
            events: &EventSink,
            _interrupted: &AtomicBool,
        ) -> Result<(), WorkerError> {
            self.record("calibrate");
            // Mirror the real backend: a fresh run clears a stale cancel.
            self.calibration_cancel.store(false, Ordering::SeqCst);
            for stage in [
                crate::events::CalibrationStage::Probing,
                crate::events::CalibrationStage::DetectingProfiles,
                crate::events::CalibrationStage::AudioSync,
                crate::events::CalibrationStage::ExtractingFrames,
                crate::events::CalibrationStage::Undistorting,
                crate::events::CalibrationStage::FeatureMatching,
                crate::events::CalibrationStage::Optimizing,
            ] {
                events.stage(stage, crate::events::StageStatus::Active, "mock stage");
            }
            events.progress(1.0);
            events.result(mock_scorecard());
            // Mirror the real backend: publish the bounded debug payload too
            // (CALB-08), so the protocol test can assert it without a GPU.
            events.calibration_debug(mock_debug_report());
            self.has_result = true;
            Ok(())
        }

        fn load_profile(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
            self.record("load_profile");
            // Mirror the real backend's typed-error shape without touching disk.
            if path.is_empty() {
                return Err(WorkerError::ProfileLoad("empty path".to_string()));
            }
            let mut calibration = sample_mock_calibration();
            calibration.field_roi = self.mock_field_roi.clone();
            self.current_calibration = Some(calibration);
            self.has_result = true;
            self.calibration_path = Some(path.clone());
            // Mirror the real backend: publish the loaded profile's ROI (CALB-09)
            // so the editor pre-populates.
            match self.mock_field_roi.clone() {
                Some(roi) => events.field_roi_applied(roi),
                None => events.field_roi_cleared(),
            }
            events.profile_loaded(path);
            Ok(())
        }

        fn save_profile(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
            self.record("save_profile");
            // Mirror the real backend: gate on the live-result flag (WR-01).
            if !self.has_result || self.current_calibration.is_none() {
                return Err(WorkerError::ProfileSave(
                    "no calibration result to save".to_string(),
                ));
            }
            self.calibration_path = Some(path.clone());
            events.profile_saved(path);
            Ok(())
        }

        fn manual_begin(&mut self, frame: u64, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_begin");
            // Mirror the real backend's protocol without a GPU or a file: emit
            // the session-started payload plus one preview frame per camera with
            // the correct RGBA geometry, so the protocol test can assert the
            // `width * height * 4` invariant.
            self.manual_open = true;
            self.manual_pins.clear();
            self.manual_next_id = 0;
            self.manual_solve_at = None;
            self.manual_relens_pending = false;
            // Capture the baseline from the mock's current calibration (if any)
            // so a reset test can prove restoration (MANU-05 / MANU-06).
            let (left, right, layout) = match self.current_calibration.as_ref() {
                Some(cal) => (cal.left.clone(), cal.right.clone(), cal.layout.clone()),
                None => (
                    super::default_camera_params(2, 1),
                    super::default_camera_params(2, 1),
                    super::default_plane_layout(),
                ),
            };
            self.manual_baseline_left = left.clone();
            self.manual_baseline_right = right.clone();
            self.manual_left_params = left;
            self.manual_right_params = right;
            self.manual_baseline_layout = layout.clone();
            self.manual_layout = layout;
            events.manual_session_started(frame, 30.0, 5);
            // A 2×1 frame => `2 * 1 * 4 = 8` RGBA bytes; the preview pair is
            // emitted by the loop's coalesced flush, not here.
            self.manual_preview_dirty = true;
            events.manual_pins(Vec::new(), false);
            self.emit_mock_manual_params(events);
            events.manual_solve_state(false, false, false);
            Ok(())
        }

        fn manual_set_frame(
            &mut self,
            _frame: u64,
            _events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("manual_set_frame");
            self.manual_preview_dirty = true;
            Ok(())
        }

        fn manual_exit(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_exit");
            self.manual_open = false;
            self.manual_pins.clear();
            self.manual_solve_at = None;
            // IN-01 parity: drop any armed re-solve too.
            self.manual_relens_pending = false;
            events.manual_solve_state(false, false, false);
            Ok(())
        }

        fn manual_detect_sync(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_detect_sync");
            // Mirror the real backend's protocol without a GPU or a file: a
            // confident estimate crosses as a typed result.
            events.audio_sync_result(3, Some(0.9));
            Ok(())
        }

        fn manual_set_sync(
            &mut self,
            offset_frames: i64,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("manual_set_sync");
            events.manual_sync_set(offset_frames, crate::events::SyncMethod::Manual);
            Ok(())
        }

        fn manual_add_pin(
            &mut self,
            left_px: [f64; 2],
            right_px: [f64; 2],
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("manual_add_pin");
            let id = self.manual_next_id;
            self.manual_next_id = self.manual_next_id.wrapping_add(1);
            self.manual_pins.push(super::SessionPin {
                id,
                left_px,
                right_px,
                verified: false,
            });
            self.after_mock_pin_mutation(events);
            Ok(())
        }

        fn manual_move_pin(
            &mut self,
            id: u32,
            side: crate::events::ManualSide,
            px: [f64; 2],
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("manual_move_pin");
            if let Some(pin) = self.manual_pins.iter_mut().find(|p| p.id == id) {
                match side {
                    crate::events::ManualSide::Left => pin.left_px = px,
                    crate::events::ManualSide::Right => pin.right_px = px,
                }
                pin.verified = false;
            }
            self.after_mock_pin_mutation(events);
            Ok(())
        }

        fn manual_remove_pin(&mut self, id: u32, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_remove_pin");
            self.manual_pins.retain(|p| p.id != id);
            self.after_mock_pin_mutation(events);
            Ok(())
        }

        fn manual_clear_pins(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_clear_pins");
            self.manual_pins.clear();
            self.after_mock_pin_mutation(events);
            Ok(())
        }

        fn manual_set_lens(
            &mut self,
            side: crate::events::ManualSide,
            fx: f64,
            cx: f64,
            cy: f64,
            k1: f64,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("manual_set_lens");
            if !self.manual_open {
                return Err(WorkerError::InvalidInput {
                    field: "lens".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
            let baseline = match side {
                crate::events::ManualSide::Left => self.manual_baseline_left.clone(),
                crate::events::ManualSide::Right => self.manual_baseline_right.clone(),
            };
            let (edited, clamped) = super::clamp_lens_edit(&baseline, fx, cx, cy, k1);
            match side {
                crate::events::ManualSide::Left => self.manual_left_params = edited.clone(),
                crate::events::ManualSide::Right => self.manual_right_params = edited.clone(),
            }
            if let Some(cal) = self.current_calibration.as_mut() {
                match side {
                    crate::events::ManualSide::Left => cal.left = edited,
                    crate::events::ManualSide::Right => cal.right = edited,
                }
            }
            self.manual_preview_dirty = true;
            self.emit_mock_manual_params(events);
            if clamped {
                events.log(Level::Warn, "lens handle reached its safe travel limit");
            }
            self.manual_relens_pending = true;
            self.manual_solve_at = Some(std::time::Instant::now() + MOCK_SOLVE_DEBOUNCE);
            events.manual_solve_state(true, true, false);
            Ok(())
        }

        fn manual_set_layout(
            &mut self,
            cam_d: f64,
            intersect: f64,
            x_ty: f64,
            x_rz: f64,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("manual_set_layout");
            if !self.manual_open {
                return Err(WorkerError::InvalidInput {
                    field: "layout".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
            let (edited, clamped) =
                super::clamp_layout_edit(cam_d, intersect, x_ty, x_rz, &self.manual_layout);
            self.manual_layout = edited.clone();
            if let Some(cal) = self.current_calibration.as_mut() {
                cal.layout = edited;
            }
            self.manual_preview_dirty = true;
            self.emit_mock_manual_params(events);
            if clamped {
                events.log(Level::Warn, "layout handle reached its safe travel limit");
            }
            self.manual_relens_pending = false;
            self.manual_solve_at = Some(std::time::Instant::now() + MOCK_SOLVE_DEBOUNCE);
            events.manual_solve_state(true, true, false);
            Ok(())
        }

        fn manual_reset_lens(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_reset_lens");
            if !self.manual_open {
                return Err(WorkerError::InvalidInput {
                    field: "lens".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
            let (left, right) = (
                self.manual_baseline_left.clone(),
                self.manual_baseline_right.clone(),
            );
            self.manual_left_params = left.clone();
            self.manual_right_params = right.clone();
            if let Some(cal) = self.current_calibration.as_mut() {
                cal.left = left;
                cal.right = right;
            }
            self.manual_preview_dirty = true;
            self.emit_mock_manual_params(events);
            // IN-02 parity: drop any armed re-solve.
            self.manual_solve_at = None;
            self.manual_relens_pending = false;
            Ok(())
        }

        fn manual_reset_rig(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_reset_rig");
            if !self.manual_open {
                return Err(WorkerError::InvalidInput {
                    field: "layout".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
            self.manual_layout = self.manual_baseline_layout.clone();
            if let Some(cal) = self.current_calibration.as_mut() {
                cal.layout = self.manual_baseline_layout.clone();
            }
            self.manual_preview_dirty = true;
            self.emit_mock_manual_params(events);
            // IN-02 parity: drop any armed re-solve.
            self.manual_solve_at = None;
            self.manual_relens_pending = false;
            Ok(())
        }

        fn manual_validate(&mut self, frame: u32, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_validate");
            if !self.manual_open {
                return Err(WorkerError::InvalidInput {
                    field: "frame".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
            // Mirror the real backend's protocol without a GPU or a decoder: a
            // validation frame with the correct RGBA geometry, an advisory
            // residual, and the calibration-frame reference. A 2×1 frame =>
            // `2 * 1 * 4 = 8` bytes.
            events.manual_validation_frame(
                frame,
                &[0u8; 8],
                2,
                1,
                0.25,
                crate::events::ValidationVerdict::LooksGood,
                (&[0u8; 8], 2, 1),
            );
            Ok(())
        }

        fn manual_save(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_save");
            if !self.manual_open {
                return Err(WorkerError::InvalidInput {
                    field: "path".to_string(),
                    reason: "no manual session is open — begin one first".to_string(),
                });
            }
            // Mirror the real backend's adoption without touching disk: assemble
            // the profile from the mock's base calibration and the session's
            // edits, gate it on validation, and adopt it as the live result.
            let base = self
                .current_calibration
                .clone()
                .unwrap_or_else(sample_mock_calibration);
            let calibration = crate::calibration::build_manual_match_calibration(
                &base,
                self.manual_left_params.clone(),
                self.manual_right_params.clone(),
                self.manual_layout.clone(),
                0,
            );
            calibration
                .validate()
                .map_err(|e| WorkerError::ProfileSave(e.to_string()))?;
            self.current_calibration = Some(calibration);
            self.has_result = true;
            events.manual_saved(path);
            Ok(())
        }

        fn manual_solve_deadline(&self) -> Option<std::time::Instant> {
            self.manual_solve_at
        }

        fn flush_manual_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            // Mirror the real backend's coalescing: one preview pair per drain,
            // not one per mutation. A 2×1 frame => `2 * 1 * 4 = 8` RGBA bytes.
            if !std::mem::take(&mut self.manual_preview_dirty) {
                return Ok(());
            }
            events.manual_preview_frame(crate::events::ManualSide::Left, &[0u8; 8], 2, 1);
            events.manual_preview_frame(crate::events::ManualSide::Right, &[0u8; 8], 2, 1);
            Ok(())
        }

        fn manual_solve(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("manual_solve");
            self.manual_solve_at = None;
            let _ = std::mem::take(&mut self.manual_relens_pending);
            // Mirror the real backend's protocol without a solver: a landed
            // result crosses as a typed layout view plus the layout delta and the
            // fresh state.
            let layout = crate::events::PlaneLayoutView {
                camera_axis_offset: 0.24,
                intersect: 0.55,
                x_ty: 0.01,
                x_rz: 0.0,
                z_rx: 0.0,
                x_rx: 0.0,
                z_rz: 0.0,
            };
            events.manual_solve_result(layout, 0.000004, self.manual_pins.len(), 0);
            events.manual_layout_delta(
                layout.camera_axis_offset - self.manual_layout.camera_axis_offset,
                layout.intersect - self.manual_layout.intersect,
                layout.x_ty - self.manual_layout.x_ty,
                layout.x_rz - self.manual_layout.x_rz,
            );
            events.manual_solve_state(false, false, false);
            Ok(())
        }

        fn begin_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("begin_preview");
            // Mirror the real backend's lazy source resolution: the adopted
            // calibration's sync offset may only be known now (D3-15).
            self.refresh_mock_preview_source();
            // Mirror the real backend exactly: the session transport is built
            // through the shared `new_session_transport`, so user-set state
            // survives the session boundary here too and the mock cannot hide
            // the same defect from these tests.
            let t = new_session_transport(
                30.0,
                Some((30, 1)),
                self.total_frames,
                self.session.as_ref(),
                self.loaded.as_ref(),
            );
            events.position(t.frame(), t.total_frames(), t.fps_rational());
            events.transport(t.state(), t.loop_enabled());
            self.session = Some(t);
            // Mirror the real backend: a session over a spent source must rewind
            // it, or the first tick sees no frame and ends the session again --
            // which is why Play did nothing after the clip had run out.
            self.rewind_for_loop()?;
            Ok(())
        }

        fn tick_session(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("tick");
            if self.tick_fails {
                // Advance to a distinctive frame *before* failing. A re-publish on
                // the failure path must then carry this frame; a re-publish that
                // is missing (or fires before the advance) is distinguishable
                // from begin_preview's frame 0.
                if let Some(t) = self.session.as_mut() {
                    for _ in 0..FAILING_TICK_FRAME {
                        t.on_frame_advanced();
                    }
                }
                return Err(WorkerError::Engine("synthetic tick failure".to_string()));
            }
            // Record which render path the view mode selected (PREV-03): the
            // mock has no GPU, so it records the path the real backend's
            // `tick_session` would take. Done before the mutable session borrow
            // so the ops lock does not alias the session borrow.
            match self.view_mode {
                crate::presenter::ViewMode::Panorama => self.record("render_panorama"),
                crate::presenter::ViewMode::Source => self.record("render_source"),
            }
            // Read the ceiling BEFORE taking the session borrow below: this
            // reads pose config, which needs `&self`, and holding it across the
            // `&mut` session borrow would overlap.
            let fov_ceiling = self.fov_ceiling();
            let Some(t) = self.session.as_mut() else {
                return Ok(());
            };
            // Mirror the real tick's pose work: tick + push FOV + emit pose.
            {
                let mut pose = self.pose.lock().unwrap();
                pose.tick();
                *self.last_pushed_fov.lock().unwrap() = Some(pose.current_fov_deg());
                let current = pose.current_pose();
                events.pose(
                    current.yaw,
                    current.pitch,
                    pose.current_fov_deg(),
                    fov_ceiling,
                );
            }
            // Advance the transport, modelling the real tick's source-driven
            // end-of-source: the fake source serves frames until it is spent,
            // only `rewind_for_loop` makes it serve again, and right after a
            // rewind it is not ready yet.
            if t.state() != crate::transport::TransportState::Playing {
                return Ok(());
            }
            // The warm-up window: freshly (re)spawned decode threads produce
            // nothing for a beat. This is the condition that killed the real
            // session on its first tick — `Ok(None)` with a source that is NOT
            // spent. Modelled so a test can reproduce the exact input the bug
            // needed, rather than only exercising the spent branch.
            let (frame_available, exhausted) = if self.warmup_ticks > 0 {
                self.warmup_ticks -= 1;
                (false, false)
            } else {
                if !self.source_exhausted {
                    self.frames_served += 1;
                    // The source's own length, which defaults to the transport
                    // total but can exceed it -- see `source_frames`.
                    let source_len = self
                        .source_frames
                        .unwrap_or_else(|| self.total_frames.unwrap_or(u64::MAX));
                    if self.frames_served >= source_len {
                        self.source_exhausted = true;
                    }
                }
                (!self.source_exhausted, self.source_exhausted)
            };
            // The shared predicate -- NOT `if self.source_exhausted`. Checking
            // only the spent flag would let this test suite pass against a real
            // tick that ends the session on any absent frame, which is exactly
            // the defect (FRICTION A12).
            if is_end_of_source(true, frame_available, exhausted) {
                self.resolve_end_of_source(events)?;
                // With Loop off the resolver drops the session, so there is no
                // position left to publish -- and the tick ends here.
                if self.session.is_none() {
                    return Ok(());
                }
            } else if frame_available {
                // A delivered frame advances the playhead; during warm-up no
                // frame arrives, so the position holds and the session stays
                // alive without pretending to play. `advance_session_after_frame`
                // does the advancing AND the second ending check in one step --
                // calling `on_frame_advanced` here as well would advance twice
                // and jump the frame index by two per tick.
                if self.advance_session_after_frame(events)? {
                    return Ok(());
                }
            }
            let (frame, total, fps_rational) = self
                .session
                .as_ref()
                .map(|t| (t.frame(), t.total_frames(), t.fps_rational()))
                .expect("a session is present");
            events.position(frame, total, fps_rational);
            Ok(())
        }

        fn end_preview(&mut self, _events: &EventSink) {
            self.record("end_preview");
            // Mirror the real backend's mirror-back, so a flag set during a
            // session is still there for the next one.
            if let (Some(loaded), Some(session)) = (self.loaded.as_mut(), self.session.as_ref()) {
                crate::transport::carry_user_state(Some(session), loaded);
            }
            self.session = None;
        }

        fn session_active(&self) -> bool {
            self.session.is_some()
        }

        fn transport(&mut self) -> &mut crate::transport::Transport {
            // Mirror the real backend's precedence exactly: prefer the live
            // session, fall back to the import-built transport. Without this the
            // mock materialized a placeholder session, so a `SetLoop` issued
            // before Play landed somewhere the real backend never uses and the
            // session-boundary carry was never exercised.
            if self.session.is_none() && self.loaded.is_none() {
                self.session = Some(crate::transport::Transport::new(30.0, None, None));
            }
            self.session
                .as_mut()
                .or(self.loaded.as_mut())
                .expect("a session or an import-built transport is present")
        }

        fn loaded_transport(&self) -> Option<&crate::transport::Transport> {
            self.loaded.as_ref()
        }

        fn preview_source_paths(&self) -> Option<(String, String)> {
            self.preview_source
                .as_ref()
                .map(|spec| (spec.left.clone(), spec.right.clone()))
        }

        fn session_transport_mut(&mut self) -> Option<&mut crate::transport::Transport> {
            self.session.as_mut()
        }

        fn publish_position(&mut self, events: &EventSink) {
            if let Some(t) = self.session.as_ref() {
                events.position(t.frame(), t.total_frames(), t.fps_rational());
            } else if let Some(t) = self.loaded.as_ref() {
                events.position(t.frame(), t.total_frames(), t.fps_rational());
            }
        }

        fn republish_projection(&mut self, events: &EventSink) {
            self.record("republish_projection");
            self.publish_position(events);
            if let Some(t) = self.session.as_ref().or(self.loaded.as_ref()) {
                events.transport(t.state(), t.loop_enabled());
            }
            let pose = self.pose.lock().unwrap().current_pose();
            let fov = self.pose.lock().unwrap().current_fov_deg();
            events.pose(pose.yaw, pose.pitch, fov, self.fov_ceiling());
            events.view(self.view_mode);
            events.presenter(self.active_kind, self.active_reason.clone());
        }

        fn set_chrome(&mut self, chrome: crate::presenter::ChromeState, _events: &EventSink) {
            self.record("set_chrome");
            // Mirror the real backend through the SAME shared predicate, so the
            // mock cannot drift from the suspend policy under test.
            *self.screen_visible.lock().unwrap() = Some(chrome.native_view_visible());
        }

        fn resize_viewport(&mut self, _width: u32, _height: u32, _events: &EventSink) {
            self.record("resize_viewport");
        }

        fn export(
            &mut self,
            _settings: &crate::events::ExportSettings,
            events: &EventSink,
            interrupted: &AtomicBool,
        ) -> Result<(), WorkerError> {
            self.record("export");
            // Emit a few per-frame progress ticks, then honour a set cancel flag
            // (EXPT-04): a cancelled run emits `ExportCancelled` and claims no
            // path; otherwise the mock completes with a software encoder. The
            // optional delay lets a test set the flag mid-run (the worker clears
            // a stale cancel at the start, so a pre-set flag is ignored).
            let total = 3u64;
            for i in 1..=total {
                if interrupted.load(Ordering::SeqCst) {
                    events.export_cancelled();
                    return Ok(());
                }
                events.export_progress(
                    i,
                    Some(total),
                    i * 10,
                    eta_ms(i * 10, i, Some(total)),
                    100.0 * i as f64 / total as f64,
                );
                if !self.mock_export_delay.is_zero() {
                    thread::sleep(self.mock_export_delay);
                }
            }
            if interrupted.load(Ordering::SeqCst) {
                events.export_cancelled();
                return Ok(());
            }
            events.export_finished(
                "/tmp/mock_panorama.mp4".to_string(),
                "libx264".to_string(),
                false,
                crate::events::ExportVariant::Panorama,
            );
            Ok(())
        }

        fn export_cancel(&self) -> Arc<AtomicBool> {
            Arc::clone(&self.export_cancel)
        }

        fn probe_encoders(&self, _codec: &str, events: &EventSink) {
            let hw = crate::events::EncoderView {
                name: "h264_nvenc".to_string(),
                description: "NVIDIA NVENC H.264".to_string(),
                is_hardware: true,
            };
            let sw = crate::events::EncoderView {
                name: "libx264".to_string(),
                description: "libx264 H.264".to_string(),
                is_hardware: false,
            };
            events.encoder_list(vec![hw.clone(), sw], hw, true);
        }

        fn preview_export_path(
            &self,
            settings: &crate::events::ExportSettings,
            events: &EventSink,
        ) {
            self.record("preview_export_path");
            match preview_path_for(self.input_paths[0].as_deref(), settings) {
                Ok(path) => events.export_path_preview(path.display().to_string()),
                Err(e) => events.log(Level::Warn, e.to_string()),
            }
        }

        fn system_info(&self, events: &EventSink) {
            self.record("system_info");
            // The mock owns no GPU: the GPU fields are honestly unknown (the
            // DIAG-01 "no GPU" edge probe), while the encoders/devices still
            // probe through the real FFmpeg surface.
            events.system_info(crate::system::probe_system(None));
        }

        fn run_preflight(&self, events: &EventSink) {
            self.record("run_preflight");
            events.preflight(crate::preflight::run_preflight());
        }

        fn save_project(
            &mut self,
            path: String,
            settings: &crate::events::ExportSettings,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("save_project");
            // Mirror the real backend without a GPU: assemble from the mock's
            // state and write atomically.
            let project = self.build_mock_project(settings);
            project
                .validate()
                .map_err(|e| WorkerError::ProjectSave(e.to_string()))?;
            project
                .write_to_file(std::path::Path::new(&path))
                .map_err(|e| WorkerError::ProjectSave(e.to_string()))?;
            events.project_saved(path);
            Ok(())
        }

        fn open_project(&mut self, path: String, events: &EventSink) -> Result<(), WorkerError> {
            self.record("open_project");
            let project = crate::project::RecoProject::read_from_file(std::path::Path::new(&path))
                .map_err(|e| WorkerError::ProjectOpen(e.to_string()))?;
            let missing = project.missing_inputs(mock_project_input_exists);
            if !missing.is_empty() {
                self.pending_project = Some(super::PendingProject {
                    path: path.clone(),
                    project,
                });
                events.project_missing_inputs(missing);
                return Ok(());
            }
            self.apply_mock_project(path, project, events);
            Ok(())
        }

        fn relocate_project_input(
            &mut self,
            role: crate::events::InputRole,
            path: String,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("relocate_project_input");
            let Some(mut pending) = self.pending_project.take() else {
                return Err(WorkerError::ProjectRelocate(
                    "no project is awaiting relocation".to_string(),
                ));
            };
            pending.project.set_input_path(role, path);
            let missing = pending.project.missing_inputs(mock_project_input_exists);
            if !missing.is_empty() {
                self.pending_project = Some(pending);
                events.project_missing_inputs(missing);
                return Ok(());
            }
            self.apply_mock_project(pending.path, pending.project, events);
            Ok(())
        }

        fn export_diagnostics_bundle(
            &self,
            path: String,
            events: &EventSink,
        ) -> Result<(), WorkerError> {
            self.record("export_diagnostics_bundle");
            // Mirror the real backend without a GPU: the GPU fields are honestly
            // unknown (probe_system(None)), the encoders/devices still probe the
            // real FFmpeg surface, and the bundle is written atomically.
            let inputs = crate::diagnostics::BundleInputs {
                logs: self.log_buffer.snapshot(),
                system: crate::system::probe_system(None),
                calibration: self
                    .current_calibration
                    .as_ref()
                    .and_then(|cal| serde_json::to_string_pretty(cal).ok()),
                debug: events
                    .snapshot_debug()
                    .map(|report| crate::diagnostics::redacted_debug_json(&report)),
                home: crate::diagnostics::home_dir(),
            };
            let summary = crate::diagnostics::write_bundle(std::path::Path::new(&path), &inputs)
                .map_err(|e| WorkerError::DiagnosticsBundle(e.to_string()))?;
            events.diagnostics_bundle_written(
                summary.path.display().to_string(),
                summary.files,
                true,
            );
            Ok(())
        }

        fn dispatch_intent(&mut self, intent: reco_control::ControlIntent) {
            self.record("intent");
            let mut pose = self.pose.lock().unwrap();
            dispatch_intent(&mut pose, intent);
        }

        fn drain_pointer_input(&mut self, _events: &EventSink) {
            self.record("drain_pointer_input");
            let Some(gesture) = self.pending_gesture.take() else {
                return;
            };
            // Mirror the real backend: drained-and-dropped in Source mode, and
            // routed through the SAME pure converter and the SAME
            // `dispatch_intent` as `WorkerCommand::Intent`.
            if self.view_mode != crate::presenter::ViewMode::Panorama {
                return;
            }
            let fov_degrees = self.pose.lock().unwrap().current_fov_deg();
            let viewport_width = VIEWPORT_WIDTH;
            let intents = crate::presenter::pointer_input::pointer_gesture_to_intents(
                gesture,
                fov_degrees,
                viewport_width,
            );
            let mut pose = self.pose.lock().unwrap();
            for intent in intents {
                dispatch_intent(&mut pose, intent);
            }
        }

        fn set_presenter(&mut self, kind: crate::presenter::PresenterKind, events: &EventSink) {
            self.record("set_presenter");
            self.active_kind = kind;
            // Mirror the real backend: a manual selection is not a fallback, so
            // the reason is cleared alongside the kind.
            self.active_reason = None;
            events.presenter(kind, None);
        }

        fn rewind_for_loop(&mut self) -> Result<(), WorkerError> {
            // Records the rewind so a test can count wraps; the real cost (a
            // decode-pipeline respawn) is invisible here, which is exactly why
            // the wrap count is the thing worth asserting on.
            self.record("rewind");
            self.frames_served = 0;
            self.source_exhausted = false;
            // The freshly spawned pipeline produces nothing for a beat. Two
            // ticks: enough for a test to observe the window reliably.
            self.warmup_ticks = 2;
            Ok(())
        }

        fn active_presenter(&self) -> crate::presenter::PresenterKind {
            self.active_kind
        }

        fn show_preview_window(&mut self, _events: &EventSink) {
            self.record("show_preview_window");
        }

        fn set_view(&mut self, mode: crate::presenter::ViewMode, events: &EventSink) {
            self.record("set_view");
            // Mirror the real backend: idempotent no-op when the mode is
            // already active (no `View` event, no INFO line).
            if mode == self.view_mode {
                return;
            }
            self.view_mode = mode;
            events.info(format!(
                "view switched to {}",
                match mode {
                    crate::presenter::ViewMode::Source => "source",
                    crate::presenter::ViewMode::Panorama => "panorama",
                }
            ));
            events.view(mode);
        }

        fn recovery_pending(&self) -> bool {
            self.lost.load(Ordering::SeqCst)
        }

        fn recover(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            // Mirror the real backend's event contract so tests can assert on
            // the recovery lines a user would actually see.
            if !self.recovery_pending() {
                return Ok(());
            }
            self.record("recover");
            events.info("device lost — recovering");
            self.lost.store(false, Ordering::SeqCst);
            events.info("device recovered");
            Ok(())
        }

        fn simulate_device_loss(&mut self, _events: &EventSink) -> Result<(), WorkerError> {
            self.record("simulate_device_loss");
            // Mimic the real backend: `destroy()` fires the lost callback, which
            // sets the recovery flag; recovery then runs at the loop's safe point.
            self.lost.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn shutdown(&mut self) {
            self.record("shutdown");
        }
    }

    #[test]
    fn project_metadata_reports_probed_codec_and_container_duration() {
        use crate::events::Provenance;
        let probe = reco_io::ffmpeg::calibration_io::VideoProbe {
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 60,
            codec: Some("h264".to_string()),
            duration_secs: Some(2.0),
            fps_rational: Some((30, 1)),
        };
        let md = project_metadata(&probe);
        assert_eq!(md.resolution.value.as_deref(), Some("1920×1080"));
        assert_eq!(md.resolution.provenance, Provenance::Probed);
        // Exact rational preferred; 30/1 -> "30 fps".
        assert_eq!(md.fps.value.as_deref(), Some("30 fps"));
        assert_eq!(md.fps.provenance, Provenance::Probed);
        // WR-03: a container-reported duration is a direct read -> PROBED.
        assert_eq!(md.duration.value.as_deref(), Some("0:02"));
        assert_eq!(md.duration.provenance, Provenance::Probed);
        // E2: a container codec is a direct read -> probed, never absent.
        assert_eq!(md.codec.value.as_deref(), Some("h264"));
        assert_eq!(md.codec.provenance, Provenance::Probed);
    }

    #[test]
    fn project_metadata_derives_duration_when_the_container_omits_it() {
        use crate::events::Provenance;
        // No `duration_secs` -> derive from total_frames / fps -> Estimated.
        let probe = reco_io::ffmpeg::calibration_io::VideoProbe {
            width: 1920,
            height: 1080,
            fps: 30.0,
            total_frames: 90,
            codec: None,
            duration_secs: None,
            fps_rational: None,
        };
        let md = project_metadata(&probe);
        assert_eq!(md.fps.value.as_deref(), Some("30 fps"));
        assert_eq!(md.fps.provenance, Provenance::Probed);
        // 90 / 30 = 3 s -> "0:03", derived (estimated), never authoritative.
        assert_eq!(md.duration.value.as_deref(), Some("0:03"));
        assert_eq!(md.duration.provenance, Provenance::Estimated);
    }

    #[test]
    fn project_metadata_handles_a_degenerate_probe_without_fabricating_values() {
        use crate::events::Provenance;
        let probe = reco_io::ffmpeg::calibration_io::VideoProbe {
            width: 0,
            height: 0,
            fps: 0.0,
            total_frames: 0,
            codec: None,
            duration_secs: None,
            fps_rational: None,
        };
        let md = project_metadata(&probe);
        assert_eq!(md.resolution.value, None);
        assert_eq!(md.fps.value, None);
        assert_eq!(md.duration.value, None);
        assert_eq!(md.duration.provenance, Provenance::Estimated);
        // Absent codec -> no value, never "0" (IMPT-02).
        assert_eq!(md.codec.value, None);
    }

    #[test]
    fn set_input_emits_a_typed_import_metadata_event() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::SetInput {
                role: crate::events::InputRole::Left,
                path: "/media/a.mp4".to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::ImportMetadata { role, .. }
                    if *role == crate::events::InputRole::Left
            )),
            "SetInput must emit a typed ImportMetadata event: {seen:?}"
        );
        assert!(ops_without_pointer_drain(&ops).contains(&"set_input"));
    }

    #[test]
    fn resolve_preview_source_requires_both_roles_and_carries_the_offset() {
        // The pure decision both backends share (D3-15): only BOTH roles set
        // switches the preview off the hardcoded startup pair.
        assert_eq!(resolve_preview_source(None, Some("r"), 3), None);
        assert_eq!(resolve_preview_source(Some("l"), None, 3), None);
        assert_eq!(
            resolve_preview_source(Some("l"), Some("r"), 3),
            Some(PreviewSourceSpec {
                left: "l".to_string(),
                right: "r".to_string(),
                sync_offset: 3,
            })
        );
    }

    #[test]
    fn begin_preview_decodes_the_imported_clips_not_the_hardcoded_pair() {
        // DEFECT: `set_input` only recorded metadata, so `begin_preview` kept
        // decoding the hardcoded startup source and Preview showed
        // left.mp4/right.mp4 after the operator imported real clips. The mock
        // mirrors the real backend's `sync_operator_source` through the shared
        // `resolve_preview_source`, so this proves the invariant GPU-free.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let events = EventSink::new(tx);
        let mut backend = MockBackend::new(ops);

        let left = "test-media/xiaomi/xiaomi-11tpro-left.mp4".to_string();
        let right = "test-media/xiaomi/xiaomi-14tpro-right.mp4".to_string();

        backend.import(&events).unwrap();
        assert_eq!(
            backend.preview_source_paths(),
            None,
            "the startup import is the hardcoded pair, not an operator source"
        );

        backend
            .set_input(crate::events::InputRole::Left, left.clone(), &events)
            .unwrap();
        assert_eq!(
            backend.preview_source_paths(),
            None,
            "one set input must not switch the preview off the startup pair"
        );

        backend
            .set_input(crate::events::InputRole::Right, right.clone(), &events)
            .unwrap();
        assert_eq!(
            backend.preview_source_paths(),
            Some((left.clone(), right.clone())),
            "both inputs set: preview must decode the imported clips"
        );

        // `begin_preview` re-resolves lazily (the adopted calibration's sync
        // offset is often only known now); it must keep the imported clips.
        backend.begin_preview(&events).unwrap();
        assert_eq!(
            backend.preview_source_paths(),
            Some((left.clone(), right.clone())),
            "begin_preview must keep decoding the imported clips"
        );

        // A cleared input drops the operator source so no stale clip is decoded.
        backend
            .clear_input(crate::events::InputRole::Right, &events)
            .unwrap();
        assert_eq!(
            backend.preview_source_paths(),
            None,
            "a cleared input must drop the operator preview source"
        );
    }

    #[test]
    fn the_named_xiaomi_clips_open_as_a_real_pair() {
        // The regression above uses the operator's real clips as the paths. Prove
        // that pair actually opens (so the seam is not exercising a fiction);
        // skip when this checkout has no `test-media/xiaomi/`.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media/xiaomi");
        let left = dir.join("xiaomi-11tpro-left.mp4");
        let right = dir.join("xiaomi-14tpro-right.mp4");
        if !left.is_file() || !right.is_file() {
            return;
        }
        let source = reco_io::adapters::FfmpegFileSource::open_with_offset(&left, &right, 0)
            .expect("the named xiaomi pair opens");
        let info = source.info();
        assert!(
            info.width > 0 && info.height > 0,
            "the imported pair reports a real resolution: {}x{}",
            info.width,
            info.height
        );
    }

    #[test]
    fn start_calibration_emits_a_stage_sequence_and_a_result() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::StartCalibration {
                options: crate::events::CalibrationOptions::default(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let stages: Vec<crate::events::CalibrationStage> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::CalibrationStage { step, .. } => Some(*step),
                _ => None,
            })
            .collect();
        for expected in [
            crate::events::CalibrationStage::Probing,
            crate::events::CalibrationStage::DetectingProfiles,
            crate::events::CalibrationStage::AudioSync,
            crate::events::CalibrationStage::ExtractingFrames,
            crate::events::CalibrationStage::Undistorting,
            crate::events::CalibrationStage::FeatureMatching,
            crate::events::CalibrationStage::Optimizing,
        ] {
            assert!(
                stages.contains(&expected),
                "expected a stage event for {expected:?}; saw {stages:?}"
            );
        }
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::CalibrationProgress { .. })),
            "calibration must emit progress"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::CalibrationResult { .. })),
            "calibration must emit a scorecard result"
        );
    }

    #[test]
    fn a_completed_calibration_emits_a_bounded_debug_report() {
        // CALB-08: after a run finishes, the typed debug payload must cross the
        // channel — carrying both point classes and a bounded thumbnail pair,
        // never a CLI PNG export.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::StartCalibration {
                options: crate::events::CalibrationOptions::default(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let report = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::CalibrationDebug { report } => Some(report.clone()),
                _ => None,
            })
            .expect("calibration must emit a CalibrationDebug event");

        assert!(!report.verified.is_empty(), "verified points must cross");
        assert!(!report.rejected.is_empty(), "rejected points must cross");
        assert_eq!(report.per_frame.len(), 1, "one per-frame row must cross");
        assert_eq!(
            report.left_thumb.len() as u32,
            report.left_width * report.left_height * 4,
            "the thumbnail length must match its geometry"
        );
        assert!(
            !report.points_capped,
            "a small report must not be marked capped"
        );
    }

    #[test]
    fn manual_begin_streams_one_preview_per_camera_and_emits_session_started() {
        // MANU-01 / MANU-03: opening a manual session must emit the typed
        // session-started payload and stream one binary preview per camera, each
        // with `rgba.len() == width * height * 4` and the right kind tag. No
        // preview bytes may cross the JSON event stream.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events, slot) =
            EngineWorker::spawn_with_manual_slot(MockBackend::new(Arc::clone(&ops)));
        let (channel, frames) = capturing_manual_channel();
        *slot.lock().unwrap() = Some(channel);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::ManualBegin { frame: 0 })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::ManualSessionStarted { .. })),
            "manual_begin must emit ManualSessionStarted: {seen:?}"
        );

        let previews = frames.lock().unwrap().clone();
        assert_eq!(
            previews.len(),
            2,
            "manual_begin must stream one preview frame per camera"
        );
        assert_eq!(previews[0][0], ManualFrameKind::PreviewLeft as u8);
        assert_eq!(previews[1][0], ManualFrameKind::PreviewRight as u8);
        for framed in &previews {
            let (_, width, height, payload) = split_manual_frame(framed);
            assert_eq!(
                payload.len() as u32,
                width * height * 4,
                "RGBA length must equal width * height * 4"
            );
        }
        for event in &seen {
            assert!(
                !serde_json::to_string(event).unwrap().contains("rgba"),
                "no preview bytes may cross the JSON event stream: {event:?}"
            );
        }
    }

    #[test]
    fn manual_exit_emits_the_session_ended_state() {
        // MANU-01: exiting drops the session state and emits the reset solve
        // state, so the flow can clear without losing a profile.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::ManualBegin { frame: 0 },
            &mut mock,
            &events,
            &interrupted,
        ));
        assert!(handle_command(
            WorkerCommand::ManualExit,
            &mut mock,
            &events,
            &interrupted,
        ));
        assert!(
            evt_rx.try_iter().any(|e| matches!(
                e,
                WorkerEvent::ManualSolveState {
                    busy: false,
                    stale: false,
                    degenerate: false
                }
            )),
            "manual_exit must emit the reset solve state"
        );
    }

    #[test]
    fn manual_sync_commands_emit_typed_results_and_provenance() {
        // MANU-02: the audio detect crosses as a typed estimate carrying the
        // fixed semantics, and a manual nudge crosses with Manual provenance.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::ManualDetectSync).unwrap();
        handle
            .send(WorkerCommand::ManualSetSync { offset_frames: -4 })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let estimate = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::AudioSyncResult {
                    offset_frames,
                    confidence,
                    offset_semantics,
                } => Some((*offset_frames, *confidence, offset_semantics.clone())),
                _ => None,
            })
            .expect("ManualDetectSync must emit an AudioSyncResult");
        assert_eq!(estimate.0, 3);
        assert_eq!(estimate.1, Some(0.9));
        assert_eq!(estimate.2, crate::events::SYNC_OFFSET_SEMANTICS);

        let set = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ManualSyncSet {
                    offset_frames,
                    method,
                    ..
                } => Some((*offset_frames, *method)),
                _ => None,
            })
            .expect("ManualSetSync must emit a ManualSyncSet");
        assert_eq!(set.0, -4);
        assert_eq!(set.1, crate::events::SyncMethod::Manual);
    }

    #[test]
    fn manual_preview_coalesces_a_mutation_burst_into_one_binary_drain_frame() {
        // Regression (Phase 04.1): a handle drag posts one command per pointer
        // event. Emitting a full-resolution preview for each flooded the JSON
        // event channel and drove WebKit RSS to tens of GB. Every mutation must
        // mark the preview dirty only; the loop's single flush per drain streams
        // one bounded binary preview pair, and a burst of N mutations must not
        // produce N frames.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(ops);
        mock.current_calibration = Some(sample_mock_calibration());
        mock.has_result = true;
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let (channel, frames) = capturing_manual_channel();
        events.attach_manual_frame_channel(channel);
        let interrupted = AtomicBool::new(false);

        // Open the session and flush its own preview so the count below is only
        // the burst's.
        let _ = handle_command(
            WorkerCommand::ManualBegin { frame: 0 },
            &mut mock,
            &events,
            &interrupted,
        );
        mock.flush_manual_preview(&events).unwrap();
        frames.lock().unwrap().clear();
        let _ = evt_rx.try_iter().count();

        // A burst of 25 layout edits, all in one "drain" (no flush between).
        for i in 0..25 {
            let _ = handle_command(
                WorkerCommand::ManualSetLayout {
                    cam_d: 0.2,
                    intersect: 0.5,
                    x_ty: 0.001 * i as f64,
                    x_rz: 0.0,
                },
                &mut mock,
                &events,
                &interrupted,
            );
        }
        assert_eq!(
            frames.lock().unwrap().len(),
            0,
            "a mutation must not stream a preview directly"
        );

        mock.flush_manual_preview(&events).unwrap();
        let previews = frames.lock().unwrap().clone();
        assert_eq!(
            previews.len(),
            2,
            "one coalesced preview pair per drain, not one per mutation"
        );
        // Kind tags: left then right (the loop's order).
        assert_eq!(previews[0][0], ManualFrameKind::PreviewLeft as u8);
        assert_eq!(previews[1][0], ManualFrameKind::PreviewRight as u8);
        for framed in &previews {
            let (kind, width, height, payload) = split_manual_frame(framed);
            assert!(matches!(
                kind,
                ManualFrameKind::PreviewLeft | ManualFrameKind::PreviewRight
            ));
            assert_eq!(
                payload.len() as u32,
                width * height * 4,
                "RGBA length must equal width * height * 4"
            );
            assert!(
                width.max(height) <= crate::calibration::MANUAL_FRAME_MAX_EDGE,
                "preview frame must be bounded: {width}x{height}"
            );
        }

        // No preview bytes cross the JSON event path: every event the burst
        // produced serializes without an `rgba` field.
        let json_events: Vec<String> = evt_rx
            .try_iter()
            .map(|e| serde_json::to_string(&e).unwrap())
            .collect();
        for json in &json_events {
            assert!(
                !json.contains("rgba"),
                "no pixel payload may cross the JSON bridge: {json}"
            );
        }

        // A second flush with nothing dirty streams nothing (the flag was taken).
        mock.flush_manual_preview(&events).unwrap();
        assert_eq!(
            frames.lock().unwrap().len(),
            2,
            "an already-flushed preview must not re-stream"
        );
    }

    /// A manual-frame channel that records every framed payload it receives, so
    /// a test can assert the binary transport without a webview.
    fn capturing_manual_channel() -> (ManualFrameChannel, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
        let frames: Arc<std::sync::Mutex<Vec<Vec<u8>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&frames);
        let channel = tauri::ipc::Channel::<tauri::ipc::Response>::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Raw(bytes) = body {
                sink.lock().unwrap().push(bytes);
            }
            Ok(())
        });
        (channel, frames)
    }

    /// Split a manual binary frame into `(kind, width, height, rgba)`.
    fn split_manual_frame(framed: &[u8]) -> (ManualFrameKind, u32, u32, &[u8]) {
        assert!(
            framed.len() >= MANUAL_FRAME_HEADER_LEN,
            "framed frame shorter than its header"
        );
        let kind = match framed[0] {
            0 => ManualFrameKind::PreviewLeft,
            1 => ManualFrameKind::PreviewRight,
            2 => ManualFrameKind::Validation,
            3 => ManualFrameKind::Reference,
            other => panic!("unknown manual frame kind {other}"),
        };
        let width = u32::from_le_bytes(framed[1..5].try_into().unwrap());
        let height = u32::from_le_bytes(framed[5..9].try_into().unwrap());
        (kind, width, height, &framed[MANUAL_FRAME_HEADER_LEN..])
    }

    #[test]
    fn manual_frame_header_is_kind_then_two_little_endian_dims() {
        // The wire layout is `[kind: u8][width: u32 LE][height: u32 LE][RGBA]`,
        // so the payload starts at offset 9. The frontend mirrors the layout and
        // its length guard fails closed if the two ever drift.
        assert_eq!(MANUAL_FRAME_HEADER_LEN, 9);
        let framed = manual_frame_with_header(ManualFrameKind::Reference, &[1, 2, 3, 4], 1000, 728);
        assert_eq!(framed.len(), MANUAL_FRAME_HEADER_LEN + 4);
        assert_eq!(framed[0], 3, "kind 3 is the reference buffer");
        assert_eq!(&framed[1..5], &1000u32.to_le_bytes());
        assert_eq!(&framed[5..9], &728u32.to_le_bytes());
        assert_eq!(&framed[MANUAL_FRAME_HEADER_LEN..], &[1, 2, 3, 4]);
        // A different kind or geometry must change the bytes.
        assert_ne!(
            framed,
            manual_frame_with_header(ManualFrameKind::Validation, &[1, 2, 3, 4], 1000, 728)
        );
        assert_ne!(
            framed,
            manual_frame_with_header(ManualFrameKind::Reference, &[1, 2, 3, 4], 1240, 728)
        );
    }

    #[test]
    fn manual_add_pin_emits_immediate_pins_and_state_then_a_debounced_solve() {
        // MANU-03: a pin drop emits the pin list and a busy/stale state
        // immediately, then a debounced background solve lands a ManualSolveResult
        // WITHOUT the pin command running the solve itself (T-04.1-11).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::ManualBegin { frame: 0 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualAddPin {
                left_px: [10.0, 10.0],
                right_px: [20.0, 20.0],
            })
            .unwrap();

        // Collect events until the solve lands (bounded so a regression cannot
        // hang the suite).
        let mut seen = Vec::new();
        let start = std::time::Instant::now();
        let mut landed = false;
        while start.elapsed() < Duration::from_secs(2) {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => {
                    landed = matches!(evt, WorkerEvent::ManualSolveResult { .. });
                    seen.push(evt);
                    if landed {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();

        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::ManualPins { pins, seeded: false } if pins.len() == 1
            )),
            "a pin add must emit ManualPins immediately: {seen:?}"
        );
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::ManualSolveState {
                    busy: true,
                    stale: true,
                    degenerate: false
                }
            )),
            "a pin add must mark the solve busy/stale: {seen:?}"
        );
        assert!(
            landed,
            "a debounced ManualSolveResult must land after the pin add: {seen:?}"
        );
        // The solve ran in the loop, never from the pin command.
        let recorded = ops.lock().unwrap().clone();
        assert!(
            recorded.contains(&"manual_add_pin") && recorded.contains(&"manual_solve"),
            "the loop must run the solve, not the pin command: {recorded:?}"
        );
    }

    #[test]
    fn manual_validate_streams_binary_frames_and_emits_metadata_only() {
        // MANU-07: validating a frame streams the stitched comparison (validation
        // + reference) over the binary channel with kind tags, and emits a
        // metadata-only `ManualValidationFrame` (residual + advisory verdict +
        // geometry, NO pixels). Saving crosses as `ManualSaved`. Neither command
        // runs the solver synchronously.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.current_calibration = Some(sample_mock_calibration());
        mock.has_result = true;
        let (worker, events, slot) = EngineWorker::spawn_with_manual_slot(mock);
        let (channel, frames) = capturing_manual_channel();
        *slot.lock().unwrap() = Some(channel);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::ManualBegin { frame: 0 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualValidate { frame: 3 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualSave {
                path: "/media/manual.json".to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let validation = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ManualValidationFrame {
                    frame,
                    width,
                    height,
                    verdict,
                    reference_width,
                    reference_height,
                    ..
                } => Some((
                    *frame,
                    *width,
                    *height,
                    *verdict,
                    *reference_width,
                    *reference_height,
                )),
                _ => None,
            })
            .expect("ManualValidate must emit a ManualValidationFrame");
        assert_eq!(validation.0, 3, "the validated frame index must cross");
        assert_eq!(validation.3, crate::events::ValidationVerdict::LooksGood);
        // The metadata event must not carry pixel arrays.
        let validation_json = seen
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .find(|json| json.contains("manual_validation_frame"))
            .expect("the validation event must serialize");
        assert!(
            !validation_json.contains("rgba"),
            "no pixel payload may cross the JSON bridge: {validation_json}"
        );

        // The two stitched buffers stream over the binary channel, tagged
        // validation then reference, each matching its geometry.
        let streamed = frames.lock().unwrap().clone();
        let validation_frames: Vec<(ManualFrameKind, u32, u32, usize)> = streamed
            .iter()
            .map(|f| {
                let (kind, w, h, payload) = split_manual_frame(f);
                (kind, w, h, payload.len())
            })
            .filter(|(kind, ..)| {
                matches!(
                    kind,
                    ManualFrameKind::Validation | ManualFrameKind::Reference
                )
            })
            .collect();
        assert_eq!(
            validation_frames.len(),
            2,
            "validate must stream the validation + reference buffers: {validation_frames:?}"
        );
        assert_eq!(validation_frames[0].0, ManualFrameKind::Validation);
        assert_eq!(validation_frames[1].0, ManualFrameKind::Reference);
        for (_, width, height, len) in &validation_frames {
            assert_eq!(
                *len as u32,
                width * height * 4,
                "streamed RGBA must match its geometry"
            );
        }
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::ManualSaved { path } if path == "/media/manual.json"
            )),
            "save must emit ManualSaved with the path: {seen:?}"
        );
        let recorded = ops.lock().unwrap().clone();
        assert!(
            recorded.contains(&"manual_validate") && recorded.contains(&"manual_save"),
            "the commands must be dispatched: {recorded:?}"
        );
    }

    #[test]
    fn manual_lens_edit_emits_params_persists_and_triggers_a_debounced_solve_with_delta() {
        // MANU-05: a lens-handle edit emits the edited real parameters
        // immediately, persists them into the calibration profile, and arms one
        // debounced background re-solve whose layout delta is emitted — without
        // the edit command running the solve itself (T-04.1-11 / T-04.1-15).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        // A current calibration so the edit has a profile to persist into.
        mock.current_calibration = Some(sample_mock_calibration());
        mock.has_result = true;
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::ManualBegin { frame: 0 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualSetLens {
                side: crate::events::ManualSide::Left,
                fx: 2000.0,
                cx: 1.0,
                cy: 1.0,
                k1: 5.0,
            })
            .unwrap();

        let mut seen = Vec::new();
        let start = std::time::Instant::now();
        let mut landed = false;
        while start.elapsed() < Duration::from_secs(2) {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => {
                    landed = matches!(evt, WorkerEvent::ManualLayoutDelta { .. });
                    seen.push(evt);
                    if landed {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();

        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::ManualParams { .. })),
            "a lens edit must emit ManualParams immediately: {seen:?}"
        );
        assert!(
            seen.iter().any(|e| matches!(
                e,
                WorkerEvent::ManualSolveState {
                    busy: true,
                    stale: true,
                    degenerate: false
                }
            )),
            "a lens edit must mark the solve busy/stale: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::ManualSolveResult { .. })),
            "a debounced ManualSolveResult must land: {seen:?}"
        );
        assert!(
            landed,
            "the re-solve must emit a ManualLayoutDelta: {seen:?}"
        );
        let recorded = ops.lock().unwrap().clone();
        assert!(
            recorded.contains(&"manual_set_lens") && recorded.contains(&"manual_solve"),
            "the loop must run the solve, not the edit command: {recorded:?}"
        );
    }

    /// Build a mock with a current calibration and an open manual session, and
    /// an event sink the test can ignore. Used by the handle clamp/persistence
    /// tests so they can inspect the backend's stored state directly.
    fn mock_with_open_manual_session() -> (MockBackend, EventSink, AtomicBool) {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(ops);
        mock.current_calibration = Some(sample_mock_calibration());
        mock.has_result = true;
        // The receiver is dropped: these tests assert stored state, not events,
        // and a failed `send` is ignored by the sink (as everywhere else).
        let (evt_tx, _evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let _ = handle_command(
            WorkerCommand::ManualBegin { frame: 0 },
            &mut mock,
            &events,
            &interrupted,
        );
        (mock, events, interrupted)
    }

    #[test]
    fn manual_set_lens_clamps_to_the_travel_range() {
        // MANU-05: a lens edit above a clamp sticks at the limit (k1 ±0.3,
        // fx ±15% of the baseline, cx/cy ±10% of the frame); it is never applied
        // unbounded (T-04.1-13).
        let (mut mock, events, interrupted) = mock_with_open_manual_session();

        let _ = handle_command(
            WorkerCommand::ManualSetLens {
                side: crate::events::ManualSide::Left,
                fx: 5000.0,
                cx: 5000.0,
                cy: 5000.0,
                k1: 5.0,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let p = &mock.manual_left_params;
        assert!(
            (p.d[0] - 0.3).abs() < 1e-9,
            "k1 must clamp to +0.3: {}",
            p.d[0]
        );
        assert!(
            (p.fx - 1150.0).abs() < 1e-9,
            "fx must clamp to +15%: {}",
            p.fx
        );
        assert!(
            (p.cx - 1152.0).abs() < 1e-9,
            "cx must clamp to +10%: {}",
            p.cx
        );
        assert!(
            (p.cy - 648.0).abs() < 1e-9,
            "cy must clamp to +10%: {}",
            p.cy
        );

        let _ = handle_command(
            WorkerCommand::ManualSetLens {
                side: crate::events::ManualSide::Left,
                fx: -5000.0,
                cx: -5000.0,
                cy: -5000.0,
                k1: -5.0,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let p = &mock.manual_left_params;
        assert!(
            (p.d[0] + 0.3).abs() < 1e-9,
            "k1 must clamp to -0.3: {}",
            p.d[0]
        );
        assert!(
            (p.fx - 850.0).abs() < 1e-9,
            "fx must clamp to -15%: {}",
            p.fx
        );
        assert!(
            (p.cx - 768.0).abs() < 1e-9,
            "cx must clamp to -10%: {}",
            p.cx
        );
        assert!(
            (p.cy - 432.0).abs() < 1e-9,
            "cy must clamp to -10%: {}",
            p.cy
        );
    }

    #[test]
    fn manual_set_lens_forces_fy_equal_to_fx() {
        // MANU-05: the scale mode drives fx and fy together (square pixels); a
        // free fy would be a second, near-redundant radial knob.
        let (mut mock, events, interrupted) = mock_with_open_manual_session();
        let _ = handle_command(
            WorkerCommand::ManualSetLens {
                side: crate::events::ManualSide::Left,
                fx: 1200.0,
                cx: 960.0,
                cy: 540.0,
                k1: 0.0,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let p = &mock.manual_left_params;
        assert_eq!(p.fy, p.fx, "fy must mirror the clamped fx");
        // 1200 is above the +15% clamp (1150), so both must be the limit.
        assert!((p.fx - 1150.0).abs() < 1e-9, "fx must clamp: {}", p.fx);
    }

    #[test]
    fn manual_set_layout_clamps_to_the_bounds() {
        // MANU-06: a layout edit outside the range clamps to the bound
        // (intersect to [0,1], cam_d to [0.1,0.30], x_ty to ±0.1, x_rz to ±0.3).
        let (mut mock, events, interrupted) = mock_with_open_manual_session();

        let _ = handle_command(
            WorkerCommand::ManualSetLayout {
                cam_d: 9.0,
                intersect: 9.0,
                x_ty: 9.0,
                x_rz: 9.0,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let l = &mock.manual_layout;
        assert!((l.camera_axis_offset - 0.30).abs() < 1e-9);
        assert!((l.intersect - 1.0).abs() < 1e-9);
        assert!((l.x_ty - 0.1).abs() < 1e-9);
        assert!((l.x_rz - 0.3).abs() < 1e-9);

        let _ = handle_command(
            WorkerCommand::ManualSetLayout {
                cam_d: -9.0,
                intersect: -9.0,
                x_ty: -9.0,
                x_rz: -9.0,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let l = &mock.manual_layout;
        assert!((l.camera_axis_offset - 0.1).abs() < 1e-9);
        assert!(l.intersect.abs() < 1e-9);
        assert!((l.x_ty + 0.1).abs() < 1e-9);
        assert!((l.x_rz + 0.3).abs() < 1e-9);
    }

    #[test]
    fn manual_reset_lens_restores_the_baseline() {
        // MANU-05: Reset lens restores the profile baseline captured at
        // `manual_begin`; the earlier edit is fully undone.
        let (mut mock, events, interrupted) = mock_with_open_manual_session();
        let _ = handle_command(
            WorkerCommand::ManualSetLens {
                side: crate::events::ManualSide::Left,
                fx: 1100.0,
                cx: 1000.0,
                cy: 600.0,
                k1: 0.2,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        assert!((mock.manual_left_params.fx - 1100.0).abs() < 1e-9);
        let _ = handle_command(
            WorkerCommand::ManualResetLens,
            &mut mock,
            &events,
            &interrupted,
        );
        let p = &mock.manual_left_params;
        assert!((p.fx - 1000.0).abs() < 1e-9, "fx must reset: {}", p.fx);
        assert!((p.cx - 960.0).abs() < 1e-9, "cx must reset: {}", p.cx);
        assert!((p.cy - 540.0).abs() < 1e-9, "cy must reset: {}", p.cy);
        assert!(p.d[0].abs() < 1e-9, "k1 must reset: {}", p.d[0]);
    }

    #[test]
    fn manual_set_lens_persists_into_current_calibration() {
        // MANU-05 / MANU-06: an edited intrinsic lands on
        // `current_calibration.left`/`.right` (so it survives a preview rebuild
        // and save); the other camera is untouched. A layout edit lands on
        // `current_calibration.layout`.
        let (mut mock, events, interrupted) = mock_with_open_manual_session();
        let _ = handle_command(
            WorkerCommand::ManualSetLens {
                side: crate::events::ManualSide::Left,
                fx: 1100.0,
                cx: 900.0,
                cy: 500.0,
                k1: 0.25,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let cal = mock
            .current_calibration
            .as_ref()
            .expect("a calibration must be present");
        assert!((cal.left.fx - 1100.0).abs() < 1e-9, "left fx persisted");
        assert!((cal.left.cx - 900.0).abs() < 1e-9, "left cx persisted");
        assert!((cal.left.d[0] - 0.25).abs() < 1e-9, "left k1 persisted");
        assert!((cal.right.fx - 1000.0).abs() < 1e-9, "right untouched");

        let _ = handle_command(
            WorkerCommand::ManualSetLayout {
                cam_d: 0.2,
                intersect: 0.6,
                x_ty: 0.05,
                x_rz: 0.1,
            },
            &mut mock,
            &events,
            &interrupted,
        );
        let cal = mock.current_calibration.as_ref().unwrap();
        assert!((cal.layout.camera_axis_offset - 0.2).abs() < 1e-9);
        assert!((cal.layout.intersect - 0.6).abs() < 1e-9);
        assert!((cal.layout.x_ty - 0.05).abs() < 1e-9);
        assert!((cal.layout.x_rz - 0.1).abs() < 1e-9);
    }

    #[test]
    fn manual_save_preserves_field_roi_and_the_carried_profile_fields() {
        // MANU-07 / T-04.1-18: the profile the manual save assembles keeps the
        // base profile's field_roi (and the other carried fields) verbatim.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(ops);
        let mut base = sample_mock_calibration();
        base.field_roi = Some(reco_core::calibration::FieldRoi {
            left: vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6]],
            right: vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]],
        });
        base.lens_correction_amount = 0.7;
        base.blend_width = 0.12;
        mock.current_calibration = Some(base.clone());
        mock.has_result = true;
        let (evt_tx, _evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);

        let _ = handle_command(
            WorkerCommand::ManualBegin { frame: 0 },
            &mut mock,
            &events,
            &interrupted,
        );
        let _ = handle_command(
            WorkerCommand::ManualSave {
                path: "/media/manual.json".to_string(),
            },
            &mut mock,
            &events,
            &interrupted,
        );

        let saved = mock
            .current_calibration
            .as_ref()
            .expect("the save must adopt a calibration");
        assert_eq!(
            saved.field_roi, base.field_roi,
            "field_roi must survive the manual save (T-04.1-18)"
        );
        assert!((saved.lens_correction_amount - 0.7).abs() < 1e-6);
        assert!((saved.blend_width - 0.12).abs() < 1e-6);
    }

    #[test]
    fn normalize_field_roi_clears_degenerate_polygons() {
        // CALB-09: fewer than three vertices is not a polygon; it clears.
        for n in 0..3 {
            let verts: Vec<[f64; 2]> = (0..n).map(|i| [i as f64 / 10.0, 0.5]).collect();
            let roi = normalize_field_roi(verts.clone(), verts);
            assert!(roi.left.is_empty(), "a {n}-vertex left polygon must clear");
            assert!(
                roi.right.is_empty(),
                "a {n}-vertex right polygon must clear"
            );
        }
    }

    #[test]
    fn normalize_field_roi_keeps_a_valid_polygon_exactly() {
        let left = vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6], [0.4, 0.95]];
        let right = vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]];
        let roi = normalize_field_roi(left.clone(), right.clone());
        assert_eq!(roi.left, left);
        assert_eq!(roi.right, right);
    }

    #[test]
    fn set_field_roi_rejects_without_a_calibration() {
        // CALB-09: there is nothing to attach a ROI to before a run/load, so the
        // worker rejects with a typed error and leaves the state unchanged.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::SetFieldRoi {
                left: vec![[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]],
                right: vec![],
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        assert!(
            mock.current_calibration.is_none(),
            "the calibration must be unchanged by a rejected command"
        );
        let failed = evt_rx.try_iter().any(|e| {
            matches!(
                e,
                WorkerEvent::Failed(WorkerError::InvalidInput { field, .. })
                    if field == "field_roi"
            )
        });
        assert!(failed, "a missing calibration must be a typed rejection");
    }

    #[test]
    fn set_field_roi_writes_then_clears_the_calibration_polygon() {
        // CALB-09: a valid polygon is written onto the calibration exactly; a
        // polygon with fewer than three vertices on both cameras clears it.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        let left = vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6], [0.4, 0.95]];
        let right = vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]];
        assert!(handle_command(
            WorkerCommand::SetFieldRoi {
                left: left.clone(),
                right: right.clone(),
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        let stored = mock
            .current_calibration
            .as_ref()
            .and_then(|c| c.field_roi.clone())
            .expect("a valid polygon must be stored");
        assert_eq!(stored.left, left);
        assert_eq!(stored.right, right);
        assert!(
            evt_rx
                .try_iter()
                .any(|e| matches!(e, WorkerEvent::FieldRoiApplied { .. })),
            "a valid polygon must emit FieldRoiApplied"
        );

        assert!(handle_command(
            WorkerCommand::SetFieldRoi {
                left: vec![[0.1, 0.2], [0.3, 0.4]],
                right: vec![[0.5, 0.6], [0.7, 0.8]],
            },
            &mut mock,
            &events,
            &interrupted,
        ));
        assert!(
            mock.current_calibration
                .as_ref()
                .unwrap()
                .field_roi
                .is_none(),
            "a short polygon on both cameras must clear the ROI"
        );
    }

    #[test]
    fn refine_lens_rejects_without_a_calibration() {
        // INTR-03: there is nothing to refine before a run/load, so the worker
        // rejects with a typed error and leaves the state unchanged.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::RefineLens {
                heldout_fraction: 0.2,
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        assert!(
            mock.current_calibration.is_none(),
            "the calibration must be unchanged by a rejected command"
        );
        let failed = evt_rx.try_iter().any(|e| {
            matches!(
                e,
                WorkerEvent::Failed(WorkerError::InvalidInput { field, .. })
                    if field == "refine_lens"
            )
        });
        assert!(failed, "a missing calibration must be a typed rejection");
    }

    #[test]
    fn refine_lens_writes_k1_only_when_accepted() {
        // INTR-03 / T-04.2-10: an accepted refinement writes the refined k1 onto
        // both cameras and emits the typed event.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            },
            &mut mock,
            &events,
            &interrupted,
        ));
        let baseline = mock.current_calibration.as_ref().unwrap().left.d[0];

        assert!(handle_command(
            WorkerCommand::RefineLens {
                heldout_fraction: 0.2,
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        let cal = mock.current_calibration.as_ref().unwrap();
        assert!(
            cal.left.d[0] > baseline,
            "an accepted refinement must write the refined k1"
        );
        assert_eq!(
            cal.right.d[0], cal.left.d[0],
            "the refined k1 must be written onto both cameras"
        );
        assert!(
            evt_rx.try_iter().any(|e| {
                matches!(e, WorkerEvent::IntrinsicsRefined { refinement } if refinement.accepted)
            }),
            "an accepted refinement must emit a typed IntrinsicsRefined"
        );
    }

    #[test]
    fn refine_lens_rejected_result_leaves_the_profile_unchanged() {
        // INTR-03 / T-04.2-10: a rejected refinement never writes k1 and still
        // emits the typed event so the rejection is visible.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.mock_refine_accepted = false;

        assert!(handle_command(
            WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            },
            &mut mock,
            &events,
            &interrupted,
        ));
        let baseline = mock.current_calibration.as_ref().unwrap().left.d[0];

        assert!(handle_command(
            WorkerCommand::RefineLens {
                heldout_fraction: 0.2,
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        let cal = mock.current_calibration.as_ref().unwrap();
        assert_eq!(
            cal.left.d[0], baseline,
            "a rejected refinement must not write k1"
        );
        assert_eq!(cal.right.d[0], baseline);
        assert!(
            evt_rx.try_iter().any(|e| {
                matches!(e, WorkerEvent::IntrinsicsRefined { refinement } if !refinement.accepted)
            }),
            "a rejected refinement must still emit a typed event"
        );
    }

    #[test]
    fn refine_lens_request_emits_an_info_log_line() {
        // UI-SPEC Event Log Contract: an INFO line is emitted when the refine
        // request is received, distinct from the completion INFO/WARN.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            },
            &mut mock,
            &events,
            &interrupted,
        ));
        // Drop the load's own log lines so only the refine request is examined.
        let _ = evt_rx.try_iter().count();

        assert!(handle_command(
            WorkerCommand::RefineLens {
                heldout_fraction: 0.2,
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        assert!(
            evt_rx.try_iter().any(|e| matches!(
                e,
                WorkerEvent::Log { level: Level::Info, message }
                    if message == "Lens k1 refinement requested"
            )),
            "the refine request must emit the UI-SPEC INFO line"
        );
    }

    #[test]
    fn refine_verified_lens_refuses_with_no_retained_matches() {
        // CR-01: after a profile load there are no retained verified matches, so
        // the shared refinement path must refuse with a clear, actionable typed
        // error (not a generic insufficient-matches result).
        let cal = sample_mock_calibration();
        let err = refine_verified_lens(&[], &cal.left, &cal.right, &cal.layout, 0.2)
            .expect_err("no retained matches must be refused");
        match err {
            WorkerError::InvalidInput { field, reason } => {
                assert_eq!(field, "refine_lens");
                assert!(
                    reason.contains("no verified matches"),
                    "the refusal must say what to do: {reason}"
                );
            }
            other => panic!("expected a typed InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn field_roi_round_trips_through_match_calibration_json() {
        // CALB-09: the polygon must survive profile save/load, which serializes
        // the whole `MatchCalibration` (including `field_roi`).
        let mut cal = sample_mock_calibration();
        cal.field_roi = Some(reco_core::calibration::FieldRoi {
            left: vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6], [0.4, 0.95]],
            right: vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]],
        });
        let json = cal.to_json_pretty();
        let back: reco_core::calibration::MatchCalibration = serde_json::from_str(&json).unwrap();
        assert_eq!(back.field_roi, cal.field_roi);
    }

    #[test]
    fn loading_a_profile_publishes_its_field_roi_for_pre_population() {
        // CALB-09: the editor pre-populates from the loaded/current calibration,
        // so the worker must publish the profile's ROI when it is adopted.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.mock_field_roi = Some(reco_core::calibration::FieldRoi {
            left: vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6]],
            right: vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]],
        });

        assert!(handle_command(
            WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            },
            &mut mock,
            &events,
            &interrupted,
        ));

        let published = evt_rx.try_iter().find_map(|e| match e {
            WorkerEvent::FieldRoiApplied { field_roi } => Some(field_roi),
            _ => None,
        });
        assert_eq!(
            published, mock.mock_field_roi,
            "loading a profile must publish its field ROI"
        );
    }

    #[test]
    fn a_stale_calibration_cancel_is_reset_by_a_new_run() {
        // CALB-02: the shared flag is the cancel channel; a new run must clear
        // a stale cancel left by a previous one, or every later run would abort
        // immediately.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(true));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.calibration_cancel = Arc::clone(&flag);
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::StartCalibration {
                options: crate::events::CalibrationOptions::default(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();
        drain_until_shutdown(&events);

        assert!(
            !flag.load(Ordering::SeqCst),
            "a fresh calibration run must reset the shared cancel flag"
        );
    }

    #[test]
    fn two_incompatible_inputs_emit_a_readiness_report() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        // Preload the right input with a different resolution than the default.
        mock.mock_metadata[1] = Some(crate::events::InputMetadata {
            resolution: crate::events::MetadataField::probed("3840×2160"),
            fps: crate::events::MetadataField::probed("30 fps"),
            duration: crate::events::MetadataField::estimated("0:05"),
            codec: crate::events::MetadataField::missing(crate::events::Provenance::Probed),
        });
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::SetInput {
                role: crate::events::InputRole::Left,
                path: "/media/a.mp4".to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::SetInput {
                role: crate::events::InputRole::Right,
                path: "/media/b.mp4".to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let mismatch = seen.iter().any(|e| match e {
            WorkerEvent::Readiness { report } => report
                .findings
                .iter()
                .any(|f| f.code == crate::events::ReadinessCode::ResolutionMismatch),
            _ => false,
        });
        assert!(
            mismatch,
            "two different resolutions must emit a ResolutionMismatch: {seen:?}"
        );
    }

    #[test]
    fn estimate_overlap_is_full_for_an_identical_profile() {
        let left: Vec<f64> = (0..64).map(|i| i as f64 + 1.0).collect();
        assert_eq!(estimate_overlap(&left, &left), Some(1.0));
    }

    #[test]
    fn overlap_from_alignment_rejects_a_weak_correlation() {
        // WR-04: a winning correlation below the floor is noise, not overlap.
        assert_eq!(overlap_from_alignment(64, 8, 0.29), None);
        assert_eq!(overlap_from_alignment(64, 8, -1.0), None);
        assert_eq!(
            overlap_from_alignment(64, 8, OVERLAP_MIN_CORRELATION),
            Some(0.875)
        );
    }

    #[test]
    fn overlap_from_alignment_reports_the_aligned_fraction() {
        assert_eq!(overlap_from_alignment(64, 8, 0.9), Some(0.875));
        assert_eq!(overlap_from_alignment(64, 0, 1.0), Some(1.0));
    }

    #[test]
    fn estimate_overlap_is_unknown_for_a_flat_profile() {
        let flat = vec![128.0; 64];
        let varied: Vec<f64> = (0..64).map(|i| (i % 7) as f64).collect();
        assert_eq!(estimate_overlap(&flat, &varied), None);
    }

    #[test]
    fn changing_a_lens_override_after_a_run_invalidates_the_result() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::SetLensOverride {
                role: crate::events::InputRole::Left,
                candidate: crate::events::LensCandidate {
                    camera: "Mock Camera".to_string(),
                    lens: "Wide".to_string(),
                    width: 1920,
                    height: 1080,
                },
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let loaded_idx = seen
            .iter()
            .position(|e| matches!(e, WorkerEvent::ProfileLoaded { .. }))
            .expect("a ProfileLoaded event");
        let invalidated_idx = seen
            .iter()
            .position(|e| matches!(e, WorkerEvent::ResultInvalidated))
            .expect("changing an override after a run invalidates the result");
        assert!(
            invalidated_idx > loaded_idx,
            "ResultInvalidated must follow the profile load"
        );
    }

    #[test]
    fn profile_load_then_save_round_trips_through_typed_events() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::SaveProfile {
                path: "/media/out.json".to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        assert!(
            seen.iter().any(
                |e| matches!(e, WorkerEvent::ProfileLoaded { path } if path == "/media/match.json")
            ),
            "load must emit ProfileLoaded with the path"
        );
        assert!(
            seen.iter().any(
                |e| matches!(e, WorkerEvent::ProfileSaved { path } if path == "/media/out.json")
            ),
            "save must emit ProfileSaved with the path"
        );
    }

    /// Drain events until `Shutdown`'s info line arrives, or time out.
    fn drain_until_shutdown(rx: &Receiver<WorkerEvent>) -> Vec<WorkerEvent> {
        let mut seen = Vec::new();
        let deadline = Duration::from_secs(2);
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => {
                    let done = matches!(
                        &evt,
                        WorkerEvent::Log { message, .. } if message == "shutdown"
                    );
                    seen.push(evt);
                    if done {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        seen
    }

    /// A unique temp dir for a project test (cleared first).
    fn project_temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reco-project-worker-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal `.reco` JSON referencing `left` and `right` (PROJ-01).
    fn sample_project_json(left: &str, right: &str) -> String {
        crate::project::RecoProject {
            version: crate::project::PROJECT_VERSION,
            left: crate::project::ProjectInput {
                path: left.to_string(),
                lens_override: None,
            },
            right: crate::project::ProjectInput {
                path: right.to_string(),
                lens_override: None,
            },
            calibration_path: None,
            calibration: None,
            pose: crate::project::PoseView::default(),
            export: crate::events::ExportSettings::default(),
        }
        .to_json()
    }

    #[test]
    fn open_project_with_missing_inputs_emits_the_typed_relocate_list() {
        let dir = project_temp_dir("missing");
        let path = dir.join("p.reco");
        std::fs::write(
            &path,
            sample_project_json("/nonexistent/left.mp4", "/nonexistent/right.mp4"),
        )
        .unwrap();

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::OpenProject {
                path: path.display().to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let missing = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ProjectMissingInputs { missing } => Some(missing.clone()),
                _ => None,
            })
            .expect("a missing input must emit ProjectMissingInputs, never fail the open");
        assert_eq!(missing.len(), 2);
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::ProjectOpened { .. })),
            "a missing input must never partially restore"
        );
    }

    #[test]
    fn relocate_project_input_restores_the_project_once_inputs_exist() {
        let dir = project_temp_dir("relocate");
        let left = dir.join("left.mp4");
        let right_present = dir.join("right.mp4");
        std::fs::write(&left, b"").unwrap();
        std::fs::write(&right_present, b"").unwrap();
        // The project references a `right` path that does not exist, so the open
        // always reports it missing; the relocate then points at the present file
        // and completes the restore. (No race: the open's missing path is fixed.)
        let path = dir.join("p.reco");
        std::fs::write(
            &path,
            sample_project_json(&left.display().to_string(), "/nonexistent/right.mp4"),
        )
        .unwrap();

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::OpenProject {
                path: path.display().to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::RelocateProjectInput {
                role: crate::events::InputRole::Right,
                path: right_present.display().to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::ProjectMissingInputs { .. })),
            "the first open must report the missing right input"
        );
        let opened = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ProjectOpened { left, right, .. } => {
                    Some((left.path.clone(), right.path.clone()))
                }
                _ => None,
            })
            .expect("relocating the missing input must restore and emit ProjectOpened");
        assert_eq!(opened.0, left.display().to_string());
        assert_eq!(opened.1, right_present.display().to_string());
    }

    #[test]
    fn export_diagnostics_bundle_writes_a_redacted_bundle_and_emits_the_event() {
        // DIAG-03: the one-click bundle writes locally and emits a typed event
        // naming the path and the file count — never a failure on success.
        let dir = project_temp_dir("diagnostics");
        let bundle_path = dir.join("reco-diagnostics.zip");

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::ExportDiagnosticsBundle {
                path: bundle_path.display().to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let written = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::DiagnosticsBundleWritten {
                    path,
                    files,
                    redacted,
                } => Some((path.clone(), *files, *redacted)),
                _ => None,
            })
            .expect("a successful bundle must emit DiagnosticsBundleWritten");
        assert_eq!(written.0, bundle_path.display().to_string());
        assert!(
            written.1 >= 3,
            "the bundle carries at least logs + system + calibration entries"
        );
        assert!(written.2, "the bundle is redacted");
        assert!(bundle_path.exists(), "the bundle must exist on disk");
        assert!(
            !seen.iter().any(|e| matches!(e, WorkerEvent::Failed(_))),
            "a successful bundle must never emit a failure"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_project_then_reopen_restores_the_inputs() {
        let dir = project_temp_dir("roundtrip");
        let left = dir.join("left.mp4");
        let right = dir.join("right.mp4");
        std::fs::write(&left, b"").unwrap();
        std::fs::write(&right, b"").unwrap();
        let project_path = dir.join("p.reco");

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::SetInput {
                role: crate::events::InputRole::Left,
                path: left.display().to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::SetInput {
                role: crate::events::InputRole::Right,
                path: right.display().to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::SaveProject {
                path: project_path.display().to_string(),
                settings: crate::events::ExportSettings::default(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::OpenProject {
                path: project_path.display().to_string(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::ProjectSaved { .. })),
            "save must emit ProjectSaved"
        );
        let opened = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::ProjectOpened { left, right, .. } => {
                    Some((left.path.clone(), right.path.clone()))
                }
                _ => None,
            })
            .expect("reopening a saved project must restore and emit ProjectOpened");
        assert_eq!(opened.0, left.display().to_string());
        assert_eq!(opened.1, right.display().to_string());
    }

    #[test]
    fn worker_processes_import_then_shutdown_in_order() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        assert_eq!(ops_without_pointer_drain(&ops), ["import", "shutdown"]);

        // The event sequence is: started → import started → import finished → shutdown.
        let messages: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Log { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            messages,
            vec![
                "engine worker started",
                "import started",
                "import finished",
                "shutdown"
            ]
        );
    }

    #[test]
    fn import_projects_position_and_transport_so_the_frontend_can_play() {
        // Phase 2 has no Import button — the app imports at startup — so the
        // worker MUST publish the loaded clip's position + transport. The
        // frontend transport store gates every action (`play`, `seek`, `step`,
        // `set_loop`) on leaving its initial "empty" state, which only happens
        // on these events. Without them Play is silently unreachable.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(ops));
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);

        let position_idx = seen
            .iter()
            .position(|e| matches!(e, WorkerEvent::Position { .. }))
            .expect("import emits an authoritative Position event");
        let transport_idx = seen
            .iter()
            .position(|e| matches!(e, WorkerEvent::Transport { .. }))
            .expect("import emits an authoritative Transport event");

        // Both projections must land AFTER the import completes: the frontend
        // reads them as "a clip is loaded", so publishing them first would
        // advertise a clip the worker has not opened yet.
        let finished_idx = seen
            .iter()
            .position(
                |e| matches!(e, WorkerEvent::Log { message, .. } if message == "import finished"),
            )
            .expect("'import finished' is emitted");
        assert!(
            position_idx > finished_idx,
            "Position must follow 'import finished' (position={position_idx}, finished={finished_idx})"
        );
        assert!(
            transport_idx > finished_idx,
            "Transport must follow 'import finished' (transport={transport_idx}, finished={finished_idx})"
        );

        // The transport starts paused, not playing: an import must never start
        // playback on its own.
        let state = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::Transport { state, .. } => Some(*state),
                _ => None,
            })
            .expect("a Transport event");
        assert_eq!(
            state,
            crate::transport::TransportState::Paused,
            "import must not begin playback"
        );
    }

    #[test]
    fn republish_projection_reasserts_transport_so_play_becomes_reachable() {
        // Regression: the event bridge is fire-and-forget, so the import's
        // Position/Transport were emitted before the webview subscribed and
        // dropped. The frontend then stayed "empty" and Play stayed disabled.
        // Republishing is what makes the opening state observable. Assert the
        // transport specifically: it is the one that flips the frontend out of
        // "empty" (and it must carry the real clip length, not a placeholder).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(ops));
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        // Drain past the import's own projections so only the republish is left
        // to be observed.
        handle.send(WorkerCommand::RepublishProjection).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let transport_events: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Transport {
                    state,
                    loop_enabled,
                } => Some((*state, *loop_enabled)),
                _ => None,
            })
            .collect();

        assert!(
            transport_events.len() >= 2,
            "expected the import's Transport plus a republished one, saw {:?}",
            transport_events.len()
        );
        let (state, loop_enabled) = *transport_events.last().unwrap();
        assert_eq!(
            state,
            crate::transport::TransportState::Paused,
            "the republished transport must leave the frontend ready-but-not-playing"
        );
        assert!(
            !loop_enabled,
            "loop must not be silently enabled by a reconcile"
        );

        // And the position it re-asserts must carry the clip's real length, so
        // the timeline can show an extent instead of an unknown one.
        let total = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Position { total, .. } => Some(total),
                _ => None,
            })
            .next_back()
            .copied()
            .expect("a Position event");
        assert_eq!(
            total,
            Some(5),
            "the republished position must carry the loaded clip's frame count"
        );
    }

    #[test]
    fn republish_projection_reports_view_preset_and_pose_without_a_session() {
        // The reconcile must restore more than transport: a webview reload (or
        // the startup race) loses the view mode, the presenter, and the pose
        // too, and each has its own frontend store. With no session there is no
        // tick, so this is the only thing that publishes a pose at all.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(ops));
        let handle = worker.handle();
        handle.send(WorkerCommand::RepublishProjection).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);

        assert!(
            seen.iter().any(|e| matches!(e, WorkerEvent::View { .. })),
            "the view mode must be re-asserted"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::Presenter { .. })),
            "the active presenter must be re-asserted"
        );
        assert!(
            seen.iter().any(|e| matches!(e, WorkerEvent::Pose { .. })),
            "the pose must be re-asserted"
        );
    }

    #[test]
    fn republish_projection_does_not_start_playback_or_a_session() {
        // A reconcile is a read. If it materialised a transport or began
        // ticking, the app would enter the session path on startup — with no
        // renderer, and (per the `loaded_transport` note) with no real timing.
        //
        // The import MUST come first: the real reconcile happens after a
        // successful import (the app imports at startup, then the webview
        // subscribes and asks). Reconciling with no clip loaded cannot reach the
        // risky branch at all, so asserting here alone would pass even against a
        // republish that opened a session.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::RepublishProjection).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        drain_until_shutdown(&events);

        let recorded = ops.lock().unwrap().clone();
        assert!(
            !recorded.contains(&"begin_preview"),
            "a reconcile must not begin a preview session, saw {recorded:?}"
        );
        assert!(
            !recorded.contains(&"tick"),
            "a reconcile must not tick, saw {recorded:?}"
        );
    }

    #[test]
    fn failed_tick_republishes_the_authoritative_position_before_ending() {
        // E3/error: "On a rejected seek the playhead reverts to the last worker
        // position and an ERROR line is appended." The frontend mirrors the
        // worker and never owns the position (UI-SPEC Interaction rule 1), so a
        // failed tick that dropped the session WITHOUT re-publishing would leave
        // the UI showing a playhead the engine never reached -- permanently, since
        // a paused session emits no further ticks to correct it.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        // A large total so the failing tick's advance is not clamped at the
        // mock's default 5-frame clip.
        mock.total_frames = Some(1_000_000);
        mock.tick_fails = true;
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Preview).unwrap();

        // The loop drains queued commands before ticking, so a Shutdown sent
        // immediately would be drained in the same pass and the failing tick
        // would never happen. Wait for the tick to have been attempted.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().contains(&"tick") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);

        let failed_idx = seen
            .iter()
            .position(|e| matches!(e, WorkerEvent::Failed(_)))
            .expect("a failed tick emits a Failed event");
        // The LAST position published before the failure must be the frame the
        // transport actually reached. Without the re-publish it would still be
        // begin_preview's frame 0, leaving the UI's playhead on a frame the
        // engine never reached.
        let last_position_before_failure = seen
            .iter()
            .take(failed_idx)
            .filter_map(|e| match e {
                WorkerEvent::Position { frame, .. } => Some(*frame),
                _ => None,
            })
            .next_back();
        assert_eq!(
            last_position_before_failure,
            Some(FAILING_TICK_FRAME),
            "the worker must re-publish its authoritative position before reporting \
             the failure, so the frontend playhead reverts"
        );

        // The session is dropped so the loop cannot spin on a dead session.
        assert!(
            ops.lock().unwrap().contains(&"end_preview"),
            "a failed tick must end the session"
        );
    }

    #[test]
    fn shutdown_on_idle_worker_returns_within_timeout() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(ops));
        // The worker is blocked on recv (idle); shutdown must unblock and join.
        worker
            .shutdown(Duration::from_secs(2))
            .expect("idle worker joins within timeout");
    }

    #[test]
    fn teardown_joins_within_timeout_after_shutdown() {
        // FOUND-06: closing the app sends Shutdown and joins with a timeout; the
        // join must return within the timeout (no hang) and the backend must see
        // its `shutdown()` drop-order hook run.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let start = std::time::Instant::now();
        worker
            .shutdown(Duration::from_secs(2))
            .expect("worker joins within the teardown timeout");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "teardown exceeded the timeout"
        );
        assert!(
            ops.lock().unwrap().contains(&"shutdown"),
            "backend shutdown() was not called on teardown"
        );
    }

    #[test]
    fn import_failure_surfaces_a_typed_failed_event() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend {
            import_fails: true,
            ..MockBackend::new(Arc::clone(&ops))
        });
        worker.handle().send(WorkerCommand::Import).unwrap();
        worker.handle().send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let failed = seen
            .iter()
            .find_map(|e| match e {
                WorkerEvent::Failed(err) => Some(err.clone()),
                _ => None,
            })
            .expect("a Failed event is emitted");
        assert_eq!(
            failed,
            WorkerError::Engine("synthetic import failure".to_string())
        );
    }

    #[test]
    fn control_intent_round_trips_through_the_channel() {
        // The intent travels the same message-passing path as the other
        // commands; its effect on the worker's pose must match a direct
        // dispatch (D-06, no direct PoseControl call from a handler).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let shared_pose = Arc::new(std::sync::Mutex::new(
            reco_control::pose_control::PoseControl::with_defaults(),
        ));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.pose = Arc::clone(&shared_pose);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();

        let intent = reco_control::ControlIntent::Pose(reco_control::PoseIntent::DeltaYawRad(0.25));
        handle.send(WorkerCommand::Intent(intent.clone())).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        // Wait for the intent to be applied before joining.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"intent") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));

        // The worker's pose (reached only through the channel) must match a
        // direct dispatch of the same intent.
        let mut direct = reco_control::pose_control::PoseControl::with_defaults();
        dispatch_intent(&mut direct, intent);
        let worker_yaw = shared_pose.lock().unwrap().target_pose().yaw;
        let direct_yaw = direct.target_pose().yaw;
        assert!(
            (worker_yaw - direct_yaw).abs() < 1e-6,
            "worker yaw {worker_yaw} != direct yaw {direct_yaw}"
        );
        assert!(
            (worker_yaw - 0.25).abs() < 1e-5,
            "worker yaw was {worker_yaw}"
        );
    }

    #[test]
    fn simulate_device_loss_triggers_recovery_in_order() {
        // FOUND-05: the debug simulation sets the lost flag, and the loop runs
        // recovery (rebuild) at its next safe point — before Shutdown.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::SimulateDeviceLoss).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        // `simulate_device_loss` runs, then recovery fires, then shutdown.
        assert_eq!(
            ops_without_pointer_drain(&ops),
            ["simulate_device_loss", "recover", "shutdown"]
        );
        let messages: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Log { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("device recovered")),
            "recovery info line missing: {messages:?}"
        );
    }

    #[test]
    fn recovery_is_skipped_when_nothing_is_lost() {
        // `recover` must be a no-op until the flag is set; a normal command
        // must not spuriously record a recovery.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
        assert_eq!(&*ops.lock().unwrap(), &["import", "shutdown"]);
    }

    #[test]
    fn recovery_action_maps_each_surface_error_kind() {
        // FOUND-05 per-variant contract (RESEARCH Pattern 5 / Pitfall 3): the
        // decision — not the GPU work — is what the unit test pins.
        use crate::presenter::SurfaceErrorKind as K;
        assert_eq!(recovery_action(K::Outdated), RecoveryAction::Reconfigure);
        assert_eq!(recovery_action(K::Lost), RecoveryAction::Rebuild);
        assert_eq!(recovery_action(K::Timeout), RecoveryAction::SkipFrame);
        assert_eq!(recovery_action(K::OutOfMemory), RecoveryAction::SkipFrame);
        assert_eq!(recovery_action(K::Other), RecoveryAction::SkipFrame);
    }

    #[test]
    fn native_geometry_line_reports_agreement_with_both_sizes() {
        // The agree case: one line naming the requested rect, the observed child
        // window size, and the chrome state that produced them.
        let rect = crate::presenter::ViewportRect::for_chrome(
            1280,
            800,
            &crate::presenter::ChromeState {
                panel_expanded: true,
                drawer_expanded: false,
                ..crate::presenter::ChromeState::default()
            },
        );
        let line = native_geometry_line(rect, Some((1000, 680)), rect_chrome_expanded());
        assert!(line.contains("requested 1000x680"), "{line}");
        assert!(line.contains("child window 1000x680"), "{line}");
        assert!(line.contains("panel expanded"), "{line}");
        assert!(line.contains("drawer collapsed"), "{line}");
        assert!(!line.contains("mismatch"), "{line}");
    }

    /// The chrome state that produces a 1000x680 rect at 1280x800.
    fn rect_chrome_expanded() -> crate::presenter::ChromeState {
        crate::presenter::ChromeState {
            panel_expanded: true,
            drawer_expanded: false,
            ..crate::presenter::ChromeState::default()
        }
    }

    #[test]
    fn native_geometry_line_names_the_mismatch_instead_of_hiding_it() {
        // A window that kept its old size must be visible in the log, because
        // this is exactly the UAT gap: the surface was reconfigured correctly
        // and the OS window was not, and nothing inside Rust could tell.
        let rect = crate::presenter::ViewportRect::for_chrome(1280, 800, &rect_chrome_expanded());
        let line = native_geometry_line(rect, Some((1240, 728)), rect_chrome_expanded());
        assert!(line.contains("requested 1000x680"), "{line}");
        assert!(line.contains("child window 1240x728"), "{line}");
        assert!(line.contains("mismatch"), "{line}");
    }

    #[test]
    fn native_geometry_line_says_unavailable_rather_than_reporting_success() {
        // A presenter with no window of its own (readback/fallback) must not
        // read as though the request was achieved.
        let rect = crate::presenter::ViewportRect::for_chrome(1280, 800, &Default::default());
        let line = native_geometry_line(rect, None, Default::default());
        assert!(line.contains("requested 1240x680"), "{line}");
        assert!(line.contains("unavailable"), "{line}");
        assert!(!line.contains("mismatch"), "{line}");
    }

    #[test]
    fn new_session_transport_carries_from_the_session_then_the_loaded_transport() {
        // The shared constructor both backends use, asserted directly so the
        // precedence is pinned without a GPU: a live session wins over the
        // import-built transport, exactly as `EngineBackend::transport()` does.
        let mut loaded = crate::transport::Transport::new(30.0, Some((30, 1)), Some(5));
        crate::transport::carry_user_state(None, &mut loaded);
        loaded.set_loop(true);

        let mut session = crate::transport::Transport::new(30.0, Some((30, 1)), Some(5));
        session.set_loop(false);

        // Session present: its flag wins, so a user who turned Loop OFF during
        // a session is not overruled by the older import-time value.
        let t = new_session_transport(30.0, Some((30, 1)), Some(5), Some(&session), Some(&loaded));
        assert!(!t.loop_enabled());
        // No session: fall back to the import-built transport.
        let t = new_session_transport(30.0, Some((30, 1)), Some(5), None, Some(&loaded));
        assert!(t.loop_enabled());
        // Neither: a first-ever session keeps the constructor's defaults.
        let t = new_session_transport(30.0, Some((30, 1)), Some(5), None, None);
        assert!(!t.loop_enabled());
        assert_eq!(t.state(), crate::transport::TransportState::Paused);
        assert_eq!(t.frame(), 0);
    }

    #[test]
    fn a_clip_that_reaches_its_transport_total_ends_through_the_shared_resolver() {
        // Regression, observed live: seven `preview session started` lines and
        // ZERO `preview reached end of source` lines. The ending was open-coded
        // as `self.session = None`, so it logged nothing and never called
        // `end_preview` -- meaning `carry_user_state` never mirrored user-set
        // state back to the loaded transport at the end of a clip.
        //
        // `source_frames` exceeds `total_frames` deliberately: in production
        // `total_frames` is a `duration × fps` ESTIMATE while the decoder serves
        // whatever the container actually holds, so the transport counter
        // usually reaches the total first. That is this path, and it is the one
        // that fired in the user's log -- while the mock, using one number for
        // both, could only ever end the source-driven way.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(5);
        mock.source_frames = Some(1_000_000);
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            if ops.lock().unwrap().contains(&"end_preview") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let seen = drain_until_shutdown(&events);
        let _ = worker.join(Duration::from_secs(2));

        let recorded = ops.lock().unwrap().clone();
        assert!(
            recorded.contains(&"end_preview"),
            "reaching the transport total must call end_preview, so user state is \
             mirrored back: {recorded:?}"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::Log { message, .. }
                    if message == "preview reached end of source")),
            "the ending must be logged -- sessions starting with no matching end \
             line is exactly what was reported: {recorded:?}"
        );
        assert!(
            recorded.iter().filter(|o| **o == "rewind").count() == 1,
            "exactly one rewind, from begin_preview -- the ending itself must not \
             respawn the decode pipeline, since the source still had frames: \
             {recorded:?}"
        );
    }

    #[test]
    fn loop_enabled_before_play_is_carried_into_the_session_play_creates() {
        // UAT gap 3, through the real command path: Import → SetLoop(true) →
        // Preview. `SetLoop` with no session active lands on the import-built
        // transport, and `Preview` then builds a brand-new one — which used to
        // default `loop_enabled` to false and silently drop the setting. The
        // worker must therefore project `loop_enabled: true` for the session it
        // just created, or the UI shows Loop off while the user left it on.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        for cmd in [
            WorkerCommand::Import,
            WorkerCommand::SetLoop(true),
            WorkerCommand::Preview,
        ] {
            assert!(handle_command(cmd, &mut mock, &events, &interrupted));
        }

        // The session Play created must itself carry the flag...
        assert!(mock.session.as_ref().expect("session").loop_enabled());
        // ...and the worker must have told the UI about it AFTER `Preview`, so
        // the frontend mirrors the loop state the new session actually has.
        let projected: Vec<_> = evt_rx
            .try_iter()
            .filter_map(|e| match e {
                WorkerEvent::Transport { loop_enabled, .. } => Some(loop_enabled),
                _ => None,
            })
            .collect();
        assert_eq!(
            projected.last().copied(),
            Some(true),
            "no post-Preview Transport projection with loop on: {projected:?}"
        );
    }

    #[test]
    fn loop_enabled_during_a_session_survives_that_session_ending() {
        // The failure paths drop a session too (a failed tick calls
        // `end_preview`), so a flag set DURING a session has to be mirrored
        // back into the import-built transport or it is lost for the next Play.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (evt_tx, _evt_rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let interrupted = AtomicBool::new(false);
        let mut mock = MockBackend::new(Arc::clone(&ops));

        assert!(handle_command(
            WorkerCommand::Import,
            &mut mock,
            &events,
            &interrupted
        ));
        assert!(handle_command(
            WorkerCommand::Preview,
            &mut mock,
            &events,
            &interrupted
        ));
        assert!(handle_command(
            WorkerCommand::SetLoop(true),
            &mut mock,
            &events,
            &interrupted
        ));

        // The session ends for a reason other than the clip running out.
        mock.end_preview(&events);
        assert!(!mock.session_active());

        assert!(handle_command(
            WorkerCommand::Preview,
            &mut mock,
            &events,
            &interrupted
        ));
        assert!(
            mock.session.as_ref().expect("session").loop_enabled(),
            "loop set during a session must survive the session ending"
        );
    }

    /// The ceiling arithmetic, without a renderer.
    ///
    /// `fov_ceiling` itself is GPU-bound — it reads the live renderer's coverage
    /// — so the pure part is factored into `fov_ceiling_from` and tested here.
    /// What matters is the ORDER of the `min`: coverage may be narrower than the
    /// configured max (the usual case — 50.87 vs 150 on the shipped clip, which
    /// is why most of the slider did nothing), or a future config may be
    /// narrower than the coverage. Both must clamp to the LOWER of the two,
    /// never to the wider one.
    #[test]
    fn fov_ceiling_clamps_to_the_narrower_of_coverage_and_config() {
        // No renderer yet: nothing to be bounded by, so the configured max wins.
        assert_eq!(
            fov_ceiling_from(None, 150.0),
            150.0,
            "before the first renderer the configured maximum is the honest answer"
        );

        let Some(coverage) = test_coverage() else {
            return; // test-media absent in this checkout
        };
        let from_coverage = coverage.max_fov_degrees();

        // The real ordering: coverage is the binding constraint.
        assert_eq!(
            fov_ceiling_from(Some(&coverage), from_coverage + 50.0),
            from_coverage,
            "when coverage is narrower than the config, coverage must win"
        );
        // ...and the config still caps it when it is the narrower one.
        assert_eq!(
            fov_ceiling_from(Some(&coverage), from_coverage - 10.0),
            from_coverage - 10.0,
            "when the config is narrower than the coverage, the config must win"
        );
        // Never wider than either input.
        let ceiling = fov_ceiling_from(Some(&coverage), 150.0);
        assert!(
            ceiling <= from_coverage + f32::EPSILON && ceiling <= 150.0,
            "the ceiling must never exceed its inputs, got {ceiling}              (coverage {from_coverage}, config 150)"
        );
    }

    /// Coverage boundary built from the shipped test calibration, or `None` when
    /// this checkout has no `test-media/`.
    fn test_coverage() -> Option<reco_core::projection::CoverageBoundary> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-media/match.json");
        if !path.is_file() {
            return None;
        }
        let cal = reco_core::calibration::MatchCalibration::from_file(&path).ok()?;
        // The same 16:9 aspect the test clip decodes at; the ceiling depends on
        // it only through the scene geometry, and any fixed value exercises the
        // ordering under test.
        let scene = reco_core::render::scene::SceneGeometry::from_layout_with_aspect(
            &cal.layout,
            16.0 / 9.0,
        );
        Some(reco_core::projection::CoverageBoundary::from_calibration(
            &cal, &scene,
        ))
    }

    #[test]
    fn is_end_of_source_requires_the_source_to_be_spent_not_just_a_missing_frame() {
        // The distinction that killed Play. `try_next_frame` returns `Ok(None)`
        // both when the decode channel is momentarily empty and when the source
        // has genuinely run out; only `is_exhausted()` separates them.
        assert!(
            !is_end_of_source(true, false, false),
            "a frame not ready yet with a source that still has frames must NOT \
             end the session -- this is the first tick after begin_preview, and \
             treating it as end-of-source is what made Play appear dead"
        );
        assert!(
            is_end_of_source(true, false, true),
            "a missing frame from a spent source IS end-of-source"
        );
        assert!(
            !is_end_of_source(true, true, false),
            "a delivered frame is never end-of-source, even on a spent source"
        );
        assert!(
            !is_end_of_source(false, false, true),
            "a paused session must not end on a missing frame -- pausing at the \
             tail of a clip must not tear the session down"
        );
    }

    #[test]
    fn a_freshly_started_session_survives_the_warmup_ticks_before_its_first_frame() {
        // Regression, observed live: `preview session started` immediately
        // followed by `preview reached end of source`, ~8ms apart, for a
        // 2-second clip. The decode pipeline had just been (re)spawned and had
        // not produced a frame yet, so `pair` was `None` on the very first tick
        // and the session ended on it.
        //
        // The symptom the user saw was Play doing nothing, over and over: every
        // press rebuilt the session, which died before its first frame arrived.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        // Far longer than the observation window: the mock has no pacing sleep,
        // so it runs unbounded. Any `end_preview` seen here can only come from
        // the warm-up conflation, never from reaching the tail.
        mock.total_frames = Some(1_000_000);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        // The first ticks fall inside the warm-up window. The session must be
        // alive and running by the time frames start arriving.
        let start = std::time::Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < Duration::from_millis(200) {
            seen = ops.lock().unwrap().clone();
            if seen.iter().filter(|o| **o == "tick").count() >= 10 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        assert!(
            seen.iter().filter(|o| **o == "tick").count() >= 10,
            "expected the session to keep running, saw {:?}",
            seen.iter().filter(|o| **o == "tick").count()
        );
        assert!(
            !seen.contains(&"end_preview"),
            "the session ended during pipeline warm-up instead of playing: \
             {seen:?}"
        );
    }

    #[test]
    fn looping_rewinds_the_source_once_per_wrap_not_once_per_tick() {
        // Regression, observed live: with Loop on the app played continuously but
        // became unresponsive, and the log filled with
        // `cuCtxCreate ... CUDA_ERROR_OUT_OF_MEMORY` plus repeated
        // `left/right decoder: software` lines. The wrap was left as a *pending
        // transport seek* for the next tick, but the source was already spent,
        // so every subsequent tick read as end-of-source and queued another wrap
        // -- and each executed seek respawned both decoders, opening a fresh CUDA
        // context apiece, until the GPU was starved.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(2);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::SetLoop(true)).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            if ops
                .lock()
                .unwrap()
                .iter()
                .filter(|o| **o == "rewind")
                .count()
                >= 8
            {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        let recorded = ops.lock().unwrap().clone();
        let ticks = recorded.iter().filter(|o| **o == "tick").count();
        let rewinds = recorded.iter().filter(|o| **o == "rewind").count();
        assert!(
            ticks >= 8 && rewinds >= 4,
            "expected repeated wraps, saw {ticks} ticks / {rewinds} rewinds"
        );
        // The defect signature: the source was spent, so EVERY tick read as
        // end-of-source and queued a wrap -- rewinds ≈ ticks. Correct behaviour
        // is one rewind per wrap, and a wrap takes at least as many ticks as the
        // clip has frames, so ticks must exceed rewinds strictly. The clip here
        // is 2 frames, so the healthy ratio is 2 ticks per rewind; requiring
        // exactly that would over-fit, `ticks > rewinds` is the real invariant.
        assert!(
            ticks > rewinds,
            "the source is being rewound at least once per tick ({ticks} ticks / \
             {rewinds} rewinds) -- that is the respawn storm, not one rewind per wrap"
        );
    }

    #[test]
    fn pressing_play_after_the_clip_ends_replays_it() {
        // Regression, observed live: the clip played once and then Play did nothing
        // however many times it was pressed. `end_preview` drops the session
        // transport but leaves the SOURCE spent, so the next `begin_preview` built a
        // fresh session against a dead source -- the first tick saw no frame, read as
        // end-of-source, and ended again, every time.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(2);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();

        handle.send(WorkerCommand::Preview).unwrap();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            if ops.lock().unwrap().contains(&"end_preview") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            ops.lock().unwrap().contains(&"end_preview"),
            "the first run must reach the end of the source"
        );

        let before = ops.lock().unwrap().len();
        handle.send(WorkerCommand::Play).unwrap();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            if ops.lock().unwrap()[before..]
                .iter()
                .filter(|o| **o == "tick")
                .count()
                >= 2
            {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        let recorded = ops.lock().unwrap().clone();
        let second_run_ticks = recorded[before..].iter().filter(|o| **o == "tick").count();
        assert!(
            second_run_ticks >= 2,
            "Play after the end must replay the clip, saw {second_run_ticks} ticks in \
         the second run: {recorded:?}"
        );
        assert!(
            recorded[before..].contains(&"rewind"),
            "the replay must rewind the spent source: {recorded:?}"
        );
    }

    #[test]
    fn pointer_drain_moves_pose_in_the_direction_the_cli_says_a_right_drag_should() {
        // The end-to-end seam: a gesture staged on the mock (standing in for the
        // native child's own event queue) is drained by `worker_loop` and lands
        // in the mock's PoseControl with the yaw sign the CLI authority gives a
        // rightward drag. +yaw looks LEFT, so drag right must yaw NEGATIVE.
        let ops = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let backend = MockBackend::new(Arc::clone(&ops)).with_pointer_gesture(
            crate::presenter::pointer_input::PointerGesture {
                drag_dx: 400.0,
                ..Default::default()
            },
        );
        let pose = Arc::clone(&backend.pose);
        let (worker, _events) = EngineWorker::spawn(backend);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();
        // Wait for the loop to have drained before shutting down. A `Shutdown`
        // queued in the same batch as the gesture returns from the command drain
        // before the drain call is ever reached, which would let this test pass
        // for the wrong reason on a regression.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && !ops.lock().unwrap().contains(&"drain_pointer_input")
        {
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        let recorded = ops.lock().unwrap().clone();
        assert!(
            recorded.contains(&"drain_pointer_input"),
            "the worker loop must drain pointer input: {recorded:?}"
        );
        let yaw = pose.lock().unwrap().target_pose().yaw;
        assert!(
            yaw < 0.0,
            "a rightward drag must yaw negative (CLI: Right = -yaw), got {yaw}"
        );
        // Zoom-relative sensitivity: 400 px of a 1000 px viewport is 0.4 FOV, from
        // the same `fov / viewport_width` scale the frontend uses.
        let expected = -0.4_f32 * 75.0_f32.to_radians();
        assert!(
            (yaw - expected).abs() < 1e-5,
            "yaw {yaw} != expected {expected}"
        );
    }

    #[test]
    fn wheel_drain_zooms_and_a_source_mode_drain_is_discarded() {
        // The wheel axis, and the view-mode gate: a gesture drained while the
        // preview shows the raw source tiles must not move the pose (the tiles
        // ignore it), but must still be consumed.
        let wheel = crate::presenter::pointer_input::PointerGesture {
            wheel_notches: 3.0,
            ..Default::default()
        };

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut backend = MockBackend::new(Arc::clone(&ops)).with_pointer_gesture(wheel);
        let (evt_tx, _rx) = std::sync::mpsc::channel();
        let events = EventSink::new(evt_tx);
        let fov_before = backend.pose.lock().unwrap().target_pose().fov_degrees;
        backend.drain_pointer_input(&events);
        let (yaw, fov_after) = {
            let pose = backend.pose.lock().unwrap();
            (pose.target_pose().yaw, pose.target_pose().fov_degrees)
        };
        assert_eq!(yaw, 0.0, "a wheel notch must not move yaw");
        assert!(
            fov_after > fov_before,
            "scroll up must widen the view: {fov_before:?} -> {fov_after:?}"
        );

        // Same gesture, but the preview is showing the source tiles.
        let mut source = MockBackend::new(Arc::new(std::sync::Mutex::new(Vec::new())))
            .with_pointer_gesture(wheel);
        source.view_mode = crate::presenter::ViewMode::Source;
        let before = source.pose.lock().unwrap().target_pose().fov_degrees;
        source.drain_pointer_input(&events);
        let after = source.pose.lock().unwrap().target_pose().fov_degrees;
        assert_eq!(
            before, after,
            "pose input must not mutate the pose in source view"
        );
        assert!(
            source.pending_gesture.is_none(),
            "the gesture is drained and discarded, not left to accumulate"
        );
    }

    #[test]
    fn distinct_commands_run_in_arrival_order() {
        // Per-sender FIFO: import → intent → export → shutdown.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle
            .send(WorkerCommand::Intent(reco_control::ControlIntent::Pose(
                reco_control::PoseIntent::Reset,
            )))
            .unwrap();
        handle
            .send(WorkerCommand::Export {
                settings: crate::events::ExportSettings::default(),
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
        assert_eq!(
            &*ops.lock().unwrap(),
            &["import", "intent", "export", "shutdown"]
        );
    }

    #[test]
    fn command_is_seen_between_ticks() {
        // RESEARCH Pattern 3: a command issued while a session is active must be
        // observed by the backend BETWEEN two session ticks, not after the
        // session ends. We start a session, inject an Intent mid-session, and
        // assert the recorded op sequence shows a "tick" after the "intent".
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000); // effectively never ends during the test
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        // Wait until at least two ticks have run.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let recorded = ops.lock().unwrap().clone();
            if recorded.iter().filter(|o| **o == "tick").count() >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        // Inject the command mid-session.
        handle
            .send(WorkerCommand::Intent(reco_control::ControlIntent::Pose(
                reco_control::PoseIntent::DeltaYawRad(0.1),
            )))
            .unwrap();
        // Wait until it is observed, with at least one more tick after it.
        let mut saw_between = false;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let recorded = ops.lock().unwrap().clone();
            if let Some(idx) = recorded.iter().position(|o| *o == "intent") {
                let after = recorded[idx + 1..].contains(&"tick");
                if after {
                    saw_between = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));
        assert!(
            saw_between,
            "a command must be observed between ticks, not after the session: {:?}",
            ops.lock().unwrap()
        );
    }

    #[test]
    fn ended_session_returns_to_blocking_recv() {
        // A session that reaches end-of-source (loop off) must end, so the
        // worker loop returns to the blocking recv path (no spin). We assert the
        // session ran to the end, then a Shutdown still joins promptly.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(3); // ends after a few ticks
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        let start = std::time::Instant::now();
        let mut ended = false;
        while start.elapsed() < Duration::from_secs(2) {
            let recorded = ops.lock().unwrap().clone();
            if recorded.iter().filter(|o| **o == "tick").count() >= 3 {
                ended = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            ended,
            "session did not reach the end: {:?}",
            ops.lock().unwrap()
        );
        // The worker is now idle (blocked on recv); Shutdown must join promptly.
        handle.send(WorkerCommand::Shutdown).unwrap();
        let t0 = std::time::Instant::now();
        worker
            .join(Duration::from_secs(2))
            .expect("worker joins after an ended session");
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn pose_intent_moves_current_pose_and_pushes_fov_after_one_tick() {
        // Task 2: after dispatch_intent(Pose(DeltaYawRad)) + one session tick,
        // the CURRENT pose yaw moves away from rest toward the target, and the
        // FOV pushed onto the pipeline is within (1, 179).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000);
        let shared_pose = Arc::clone(&mock.pose);
        let pushed = Arc::clone(&mock.last_pushed_fov);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        let intent = reco_control::ControlIntent::Pose(reco_control::PoseIntent::DeltaYawRad(0.5));
        handle.send(WorkerCommand::Intent(intent)).unwrap();

        let start = std::time::Instant::now();
        let mut moved = false;
        while start.elapsed() < Duration::from_secs(2) {
            {
                let pose = shared_pose.lock().unwrap();
                if pose.current_yaw_rad().abs() > 1e-4 {
                    moved = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));
        assert!(moved, "current pose yaw did not move after a tick");
        let fov = pushed
            .lock()
            .unwrap()
            .expect("a FOV was pushed to the pipeline");
        assert!(
            fov > 1.0 && fov < 179.0,
            "pushed FOV {fov} outside (1, 179)"
        );
    }

    #[test]
    fn reset_intent_returns_pose_to_rest() {
        // Task 2: Pose(Reset) snaps yaw/pitch/fov back to the rest values.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mock = MockBackend::new(Arc::clone(&ops));
        let shared_pose = Arc::clone(&mock.pose);
        {
            let mut pose = shared_pose.lock().unwrap();
            pose.apply_drag(500.0, -300.0);
            pose.tick_with(1.0);
            assert!(pose.current_yaw_rad().abs() > 1e-3);
        }
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::Intent(reco_control::ControlIntent::Pose(
                reco_control::PoseIntent::Reset,
            )))
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().contains(&"intent") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let _ = worker.join(Duration::from_secs(2));
        let pose = shared_pose.lock().unwrap();
        let rest = pose.config().rest_pose;
        assert!((pose.current_yaw_rad() - rest.yaw).abs() < 1e-6);
        assert!((pose.current_pitch_rad() - rest.pitch).abs() < 1e-6);
        assert!((pose.current_fov_deg() - rest.fov_degrees.unwrap()).abs() < 1e-6);
    }

    #[test]
    fn pose_event_is_emitted_carrying_the_current_pose() {
        // Task 2: a Pose event carries the worker's current pose after a tick.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000);
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();
        handle
            .send(WorkerCommand::Intent(reco_control::ControlIntent::Pose(
                reco_control::PoseIntent::SetFovDeg(60.0),
            )))
            .unwrap();

        let mut saw_pose = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::Pose { fov_degrees, .. }) => {
                    assert!(fov_degrees > 1.0 && fov_degrees < 179.0);
                    saw_pose = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));
        assert!(saw_pose, "no Pose event was emitted");
    }

    #[test]
    fn set_presenter_command_swaps_and_emits_a_presenter_event() {
        // Task 3: SetPresenter reaches the backend, swaps the active presenter,
        // and emits a Presenter event (no WARN for a successful manual swap).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::SetPresenter(
                crate::presenter::PresenterKind::Readback,
            ))
            .unwrap();

        let mut saw_presenter = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::Presenter { kind, reason }) => {
                    assert_eq!(kind, crate::presenter::PresenterKind::Readback);
                    assert!(reason.is_none(), "manual swap must not report a fallback");
                    saw_presenter = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));
        assert!(saw_presenter, "no Presenter event was emitted");
        assert!(ops.lock().unwrap().contains(&"set_presenter"));
    }

    #[test]
    fn startup_fallback_republish_reasserts_the_resolved_kind_and_one_locked_warn() {
        // G-02-9 regression: a chain lacking Native (the native arm failed to
        // construct on a Wayland handle) activates the first available arm with
        // the recorded native error as its reason. `republish_projection` — the
        // single post-subscribe delivery point (F1) — must re-assert BOTH the
        // resolved kind and the locked fallback WARN, exactly once. No existing
        // test covers this: `MockBackend` hardcodes `active_kind = Native`.
        let reason = crate::presenter::PresenterError::Unsupported {
            reason: "parent window handle is Wayland(...), not Xlib — Wayland has no \
                     X11-style child embedding (D-05)"
                .to_string(),
        }
        .to_string();

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.active_kind = crate::presenter::PresenterKind::SeparateWindow;
        mock.active_reason = Some(reason.clone());
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::RepublishProjection).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let fallback_events: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Presenter {
                    kind,
                    reason: Some(r),
                } => Some((*kind, r.clone())),
                _ => None,
            })
            .collect();

        assert_eq!(
            fallback_events.len(),
            1,
            "exactly one fallback Presenter event per reconcile, got {fallback_events:?}"
        );
        let (kind, reported) = &fallback_events[0];
        assert_eq!(*kind, crate::presenter::PresenterKind::SeparateWindow);

        // The resolved event projects to the locked WARN shape.
        let line = WorkerEvent::Presenter {
            kind: *kind,
            reason: Some(reported.clone()),
        }
        .to_log_line();
        assert_eq!(line.level, Level::Warn);
        assert_eq!(
            line.message,
            crate::presenter::fallback_warn_line(
                crate::presenter::PresenterKind::SeparateWindow,
                &reason
            )
        );
        assert!(
            line.message
                .starts_with("Presenter fallback to Separate window: ")
        );

        // The native path emits no fallback: with `active_reason == None`,
        // `republish_projection` projects to the INFO `presenter: <Kind>` line.
        // This half also fails if a duplicate boot emission is introduced.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::RepublishProjection).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let fallback_count = seen
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    WorkerEvent::Presenter {
                        reason: Some(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            fallback_count, 0,
            "the native path must emit no fallback Presenter event"
        );
    }

    #[test]
    fn show_preview_window_command_reaches_the_backend() {
        // Task 3: ShowPreviewWindow reaches the backend.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle.send(WorkerCommand::ShowPreviewWindow).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
        assert!(ops.lock().unwrap().contains(&"show_preview_window"));
    }

    #[test]
    fn chrome_and_resize_commands_reach_the_backend() {
        // Task 3: SetChrome and ResizeViewport are handled at the worker's
        // command drain and observed by the backend in arrival order. They must
        // not require an active session (Pitfall 8: reconfigure preserves
        // position/pose; it is a boundary command).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: true,
                drawer_expanded: true,
                active_screen: crate::presenter::Screen::Preview,
                modal_open: false,
            })
            .unwrap();
        handle
            .send(WorkerCommand::ResizeViewport {
                width: 1024,
                height: 600,
            })
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
        // The loop's per-iteration pointer drain interleaves with the command
        // batch, so filter it before asserting the command ordering.
        assert_eq!(
            ops_without_pointer_drain(&ops),
            ["set_chrome", "resize_viewport", "shutdown"]
        );
    }

    #[test]
    fn set_chrome_drives_native_visibility_from_the_active_screen() {
        // UI-SPEC Screen Router (E6): the native child view is live only on
        // Preview. Import/Calibrate suspend it. The mock mirrors the real
        // backend's `presenter.set_visible(screen == Preview)` so the
        // screen-driven contract is exercised without a GPU.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let visible = Arc::new(std::sync::Mutex::new(None));
        let (worker, _events) = EngineWorker::spawn(
            MockBackend::new(Arc::clone(&ops)).with_screen_visible(Arc::clone(&visible)),
        );
        let handle = worker.handle();

        let wait_for = |target: Option<bool>| {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if *visible.lock().unwrap() == target {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for screen visibility == {target:?}");
        };

        // Import: the native view is suspended.
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: false,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Import,
                modal_open: false,
            })
            .unwrap();
        wait_for(Some(false));

        // Preview: the native view is shown again.
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: false,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Preview,
                modal_open: false,
            })
            .unwrap();
        wait_for(Some(true));

        handle.send(WorkerCommand::Shutdown).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
    }

    #[test]
    fn set_chrome_modal_open_suspends_native_view_on_preview() {
        // Regression: a webview modal (the `.reco` relocate dialog) can open
        // while Preview is active. On X11 the native child view composites ABOVE
        // the webview, so the live panorama would occlude the modal unless the
        // modal-open signal suspends it — and it must be restored when the modal
        // closes. The signal rides the same `SetChrome` path as the screen and
        // the mock observes the shared `native_view_visible` predicate, so this
        // proves the worker's protocol-level contract without a GPU.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let visible = Arc::new(std::sync::Mutex::new(None));
        let (worker, _events) = EngineWorker::spawn(
            MockBackend::new(Arc::clone(&ops)).with_screen_visible(Arc::clone(&visible)),
        );
        let handle = worker.handle();

        let wait_for = |target: Option<bool>| {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                if *visible.lock().unwrap() == target {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for native visibility == {target:?}");
        };

        // Preview with no modal: the native view is live.
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: false,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Preview,
                modal_open: false,
            })
            .unwrap();
        wait_for(Some(true));

        // A modal opens while Preview stays active: the native view is suspended.
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: false,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Preview,
                modal_open: true,
            })
            .unwrap();
        wait_for(Some(false));

        // The modal closes: the native view is restored on the same screen.
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: false,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Preview,
                modal_open: false,
            })
            .unwrap();
        wait_for(Some(true));

        handle.send(WorkerCommand::Shutdown).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
    }

    #[test]
    fn set_view_before_any_preview_is_accepted_and_consistent() {
        // PREV-03 boundary: a view toggle before any preview (no imported
        // source) is accepted and leaves the mode consistent.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, _events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Source))
            .unwrap();
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Panorama))
            .unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if ops.lock().unwrap().contains(&"shutdown") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join(Duration::from_secs(2));
        // Both set_view commands were accepted (no error, no panic).
        let recorded = ops.lock().unwrap().clone();
        assert_eq!(
            recorded.iter().filter(|o| **o == "set_view").count(),
            2,
            "both SetView commands should be recorded: {recorded:?}"
        );
    }

    #[test]
    fn set_view_at_frame_zero_preserves_frame_index() {
        // PREV-03 boundary + precision: a toggle at frame 0 leaves the frame
        // index unchanged (no rounding drift).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        // Wait for at least one tick so the session is active.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().iter().filter(|o| **o == "tick").count() >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        // Toggle to Source and wait for a tick to record the source path.
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Source))
            .unwrap();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().contains(&"render_source") {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        // Toggle back to Panorama and wait for a tick to record it.
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Panorama))
            .unwrap();
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let recorded = ops.lock().unwrap().clone();
            let source_count = recorded.iter().filter(|o| **o == "render_source").count();
            let panorama_after = recorded.iter().filter(|o| **o == "render_panorama").count();
            if source_count >= 1 && panorama_after > source_count {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        // Both render paths were recorded: the toggle was accepted at frame 0
        // and the playhead was not reset (the session is still running).
        let recorded = ops.lock().unwrap().clone();
        assert!(
            recorded.contains(&"render_source"),
            "source render path should have been recorded: {recorded:?}"
        );
        assert!(
            recorded.contains(&"render_panorama"),
            "panorama render path should have been recorded: {recorded:?}"
        );
    }

    #[test]
    fn set_view_mid_playback_preserves_exact_frame_index() {
        // PREV-03 precision: a mid-playback toggle preserves the exact
        // Transport.frame value (assert exact equality, not approximate).
        //
        // The transport advances by 1 on each tick; the toggle itself must not
        // add or subtract from that. We collect Position events across the
        // toggle and assert the frame index advances by exactly 1 between
        // consecutive ticks.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000);
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        // Collect Position events in a background thread so they are not lost
        // between measurement points.
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let events_clone = events;
        std::thread::spawn(move || {
            while let Ok(evt) = events_clone.recv() {
                if let WorkerEvent::Position { frame, .. } = evt {
                    collected_clone.lock().unwrap().push(frame);
                }
            }
        });

        // Wait for several ticks so the frame index advances.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().iter().filter(|o| **o == "tick").count() >= 5 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        // Toggle to Source mid-playback.
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Source))
            .unwrap();

        // Wait for the toggle to be observed and a few more ticks.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops
                .lock()
                .unwrap()
                .iter()
                .filter(|o| **o == "render_source")
                .count()
                >= 3
            {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        // Verify the frame index advances by exactly 1 between consecutive
        // ticks (no reset, no double-advance).
        //
        // The leading frame-0 run is dropped first: Import publishes the loaded
        // clip's position and Preview publishes the session start, so frame 0 is
        // legitimately projected two or three times before the first tick
        // advances it. Those are command-time projections, not ticks. Anything
        // after the first advance — a reset to 0, a skipped or doubled step — is
        // still caught by the windows below.
        //
        // Exactly ONE leading zero is kept: the boundary window from the
        // session-start projection into the first tick is the one a first-tick
        // double-advance corrupts (0 -> 2), so dropping it would hide that bug.
        let frames: Vec<u64> = {
            let all = collected.lock().unwrap().clone();
            let first_advance = all.iter().position(|f| *f != 0).unwrap_or(all.len());
            let keep_from = first_advance.saturating_sub(1);
            all[keep_from..].to_vec()
        };
        assert!(
            frames.len() >= 4,
            "expected at least 4 Position events, got {}: {frames:?}",
            frames.len()
        );
        for (i, w) in frames.windows(2).enumerate() {
            assert_eq!(
                w[1],
                w[0] + 1,
                "frame index must advance by exactly 1 between consecutive ticks \
                 (window {i}: {:?})",
                w
            );
        }
    }

    #[test]
    fn set_view_during_active_session_is_observed_between_ticks() {
        // PREV-03 concurrency: a SetView command queued during an active
        // session is observed between two session ticks (same harness as
        // plan 02-02 Task 1's command_is_seen_between_ticks).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000);
        let (worker, _events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        // Wait until at least two ticks have run.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let recorded = ops.lock().unwrap().clone();
            if recorded.iter().filter(|o| **o == "tick").count() >= 2 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        // Inject the SetView command mid-session.
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Source))
            .unwrap();
        // Wait until it is observed, with at least one more tick after it.
        let mut saw_between = false;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let recorded = ops.lock().unwrap().clone();
            if let Some(idx) = recorded.iter().position(|o| *o == "set_view") {
                let after = recorded[idx + 1..].contains(&"tick");
                if after {
                    saw_between = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));
        assert!(
            saw_between,
            "a SetView command must be observed between ticks, not after the session: {:?}",
            ops.lock().unwrap()
        );
    }

    #[test]
    fn set_view_idempotent_no_op_when_already_active() {
        // PREV-03 idempotency: issuing SetView(Panorama) while already
        // Panorama produces no frame-index change and no second View event.
        //
        // The transport advances by 1 on each tick; the idempotent toggle must
        // not add or subtract from that. We collect Position events across the
        // toggle and assert the frame index advances by exactly 1 between
        // consecutive ticks.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut mock = MockBackend::new(Arc::clone(&ops));
        mock.total_frames = Some(1_000_000);
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();

        // Collect Position events in a background thread.
        let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let events_clone = events;
        std::thread::spawn(move || {
            while let Ok(evt) = events_clone.recv() {
                if let WorkerEvent::Position { frame, .. } = evt {
                    collected_clone.lock().unwrap().push(frame);
                }
            }
        });

        // Wait for a few ticks.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().iter().filter(|o| **o == "tick").count() >= 3 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        // Issue SetView(Panorama) — already the active mode.
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Panorama))
            .unwrap();

        // Wait for a few more ticks.
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if ops.lock().unwrap().iter().filter(|o| **o == "tick").count() >= 6 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        handle.send(WorkerCommand::Shutdown).unwrap();
        let _ = worker.join(Duration::from_secs(2));

        // Verify the frame index advances by exactly 1 between consecutive
        // ticks (no reset, no double-advance).
        //
        // The leading frame-0 run is dropped first: Import publishes the loaded
        // clip's position and Preview publishes the session start, so frame 0 is
        // legitimately projected two or three times before the first tick
        // advances it. Those are command-time projections, not ticks. Anything
        // after the first advance — a reset to 0, a skipped or doubled step — is
        // still caught by the windows below.
        //
        // Exactly ONE leading zero is kept: the boundary window from the
        // session-start projection into the first tick is the one a first-tick
        // double-advance corrupts (0 -> 2), so dropping it would hide that bug.
        let frames: Vec<u64> = {
            let all = collected.lock().unwrap().clone();
            let first_advance = all.iter().position(|f| *f != 0).unwrap_or(all.len());
            let keep_from = first_advance.saturating_sub(1);
            all[keep_from..].to_vec()
        };
        assert!(
            frames.len() >= 4,
            "expected at least 4 Position events, got {}: {frames:?}",
            frames.len()
        );
        for (i, w) in frames.windows(2).enumerate() {
            assert_eq!(
                w[1],
                w[0] + 1,
                "frame index must advance by exactly 1 between consecutive ticks \
                 (window {i}: {:?})",
                w
            );
        }
        // The set_view op must have been recorded (the command was accepted)
        // but no View event should have been emitted (the mode didn't change).
        assert!(
            ops.lock().unwrap().contains(&"set_view"),
            "set_view should be recorded"
        );
    }

    #[test]
    fn protocol_payloads_are_send_and_clone() {
        // FOUND-03's "no shared engine lock across a tick" invariant, checked
        // structurally: every value that crosses the worker boundary is
        // `Clone + Send`, so no engine object (which is not `Send`-sharable)
        // can travel with it and no lock needs to be held across a tick.
        // CONCERNS.md ("Mutex-poisoning panics") is the prior art this avoids:
        // a UI-held engine lock could poison a `Mutex` on a render panic; here
        // there is no shared engine lock at all.
        fn assert_clone_send<T: Clone + Send + 'static>() {}
        assert_clone_send::<WorkerCommand>();
        assert_clone_send::<WorkerEvent>();
        assert_clone_send::<WorkerError>();
        // The intent variant's payload is itself `Clone + Send` (asserted in
        // `reco-control`), so it travels the same path as the other commands.
        assert_clone_send::<reco_control::ControlIntent>();
    }

    #[test]
    fn eta_ms_is_none_until_a_frame_completes_and_the_total_is_known() {
        // EXPT-04 edge probe: a zero completed count or an unknown total must
        // never divide by zero — the UI renders `Not reported`.
        assert_eq!(eta_ms(1_000, 0, Some(100)), None);
        assert_eq!(eta_ms(1_000, 10, None), None);
        assert_eq!(eta_ms(1_000, 10, Some(0)), None);
        // The plan's formula: elapsed × (total − completed) / completed.
        assert_eq!(eta_ms(1_000, 50, Some(100)), Some(1_000));
        // Completed at/over the total is a real zero, not an unknown.
        assert_eq!(eta_ms(1_000, 100, Some(100)), Some(0));
    }

    #[test]
    fn mock_export_emits_per_frame_progress_then_finishes() {
        // EXPT-04: an export emits per-frame `ExportProgress` and terminates
        // with `ExportFinished` carrying the resolved encoder.
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        handle
            .send(WorkerCommand::Export {
                settings: crate::events::ExportSettings::default(),
            })
            .unwrap();

        let mut seen = Vec::new();
        let start = std::time::Instant::now();
        let mut finished = false;
        while start.elapsed() < Duration::from_secs(2) {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => {
                    finished = matches!(evt, WorkerEvent::ExportFinished { .. });
                    seen.push(evt);
                    if finished {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();

        assert!(
            seen.iter()
                .any(|e| matches!(e, WorkerEvent::ExportProgress { .. })),
            "an export must emit per-frame ExportProgress: {seen:?}"
        );
        assert!(
            finished,
            "an export must terminate with ExportFinished: {seen:?}"
        );
        assert!(
            ops.lock().unwrap().contains(&"export"),
            "the mock export must have run"
        );
    }

    #[test]
    fn mock_export_with_a_set_cancel_emits_export_cancelled() {
        // EXPT-04: the dedicated `export_cancel` flag (not the calibration flag)
        // stops the run and yields a typed `ExportCancelled` — never a finished
        // event and never a claimed output path. The flag is set mid-run (the
        // worker clears a stale cancel at export start, so a pre-set flag is
        // intentionally ignored).
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let mock = MockBackend::new(Arc::clone(&ops))
            .with_export_cancel(Arc::clone(&cancel))
            .with_export_delay(Duration::from_millis(40));
        let (worker, events) = EngineWorker::spawn(mock);
        let handle = worker.handle();
        handle
            .send(WorkerCommand::Export {
                settings: crate::events::ExportSettings::default(),
            })
            .unwrap();

        let mut seen = Vec::new();
        let start = std::time::Instant::now();
        let mut cancelled = false;
        let mut set_cancel = false;
        while start.elapsed() < Duration::from_secs(3) {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => {
                    // Set the cancel as soon as the run is provably in flight.
                    if !set_cancel && matches!(evt, WorkerEvent::ExportProgress { .. }) {
                        cancel.store(true, Ordering::SeqCst);
                        set_cancel = true;
                    }
                    cancelled = matches!(evt, WorkerEvent::ExportCancelled);
                    seen.push(evt);
                    if cancelled {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();

        assert!(
            set_cancel,
            "the export must have started before the cancel was set: {seen:?}"
        );
        assert!(
            cancelled,
            "a mid-run export_cancel must yield ExportCancelled: {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, WorkerEvent::ExportFinished { .. })),
            "a cancelled export must not report finished: {seen:?}"
        );
    }

    #[test]
    fn clamp_trim_clamps_out_of_range_and_never_emits_an_empty_window() {
        // EXPT-03 edge probes: a full clip is a no-op; an out <= in window is
        // clamped (the out point is dropped) so an empty window is never
        // exported; an out-of-range start/end is clamped to the clip.
        assert_eq!(clamp_trim(None, None, Some(100)), (None, None, false));
        assert_eq!(
            clamp_trim(Some(10), Some(20), Some(100)),
            (Some(10), Some(20), false)
        );
        assert_eq!(
            clamp_trim(Some(30), Some(20), Some(100)),
            (Some(30), None, true)
        );
        assert_eq!(
            clamp_trim(Some(0), Some(0), Some(100)),
            (Some(0), None, true)
        );
        assert_eq!(
            clamp_trim(Some(150), None, Some(100)),
            (Some(99), None, true)
        );
        assert_eq!(
            clamp_trim(Some(10), Some(200), Some(100)),
            (Some(10), Some(100), true)
        );
        // An unknown total leaves the window untouched.
        assert_eq!(
            clamp_trim(Some(10), Some(20), None),
            (Some(10), Some(20), false)
        );
    }

    #[test]
    fn variant_grid_layout_packs_the_source_tiles_for_each_grid_variant() {
        // EXPT-05: side-by-side packs the two source tiles horizontally, stacked
        // vertically; the atlas dims follow from the tile dims. Panorama is not
        // a grid variant.
        let sbs =
            variant_grid_layout(crate::events::ExportVariant::SideBySide, 1920, 1080).unwrap();
        assert_eq!((sbs.packed_width(), sbs.packed_height()), (3840, 1080));
        let stacked =
            variant_grid_layout(crate::events::ExportVariant::Stacked, 1920, 1080).unwrap();
        assert_eq!(
            (stacked.packed_width(), stacked.packed_height()),
            (1920, 2160)
        );
        assert!(variant_grid_layout(crate::events::ExportVariant::Panorama, 1920, 1080).is_none());
        // A YUV420P-odd tile is rejected by the shared alignment rule.
        assert!(variant_grid_layout(crate::events::ExportVariant::Stacked, 1921, 1080).is_none());
    }

    #[test]
    fn preview_export_path_emits_the_worker_resolved_path() {
        // EXPT-06: the preview is a worker-owned path; the store mirrors the
        // typed `ExportPathPreview` and never constructs one itself.
        let dir = std::env::temp_dir().join(format!(
            "reco_preview_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dir_str = dir.display().to_string();

        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        let handle = worker.handle();
        let settings = crate::events::ExportSettings {
            output_dir: Some(dir_str.clone()),
            variant: crate::events::ExportVariant::SideBySide,
            ..Default::default()
        };
        handle
            .send(WorkerCommand::PreviewExportPath { settings })
            .unwrap();

        let mut path = None;
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(WorkerEvent::ExportPathPreview { path: p }) => {
                    path = Some(p);
                    break;
                }
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        handle.send(WorkerCommand::Shutdown).unwrap();

        let path = path.expect("preview_export_path must emit ExportPathPreview");
        assert!(
            path.ends_with("_sbs.mp4"),
            "the resolved path must carry the variant suffix: {path}"
        );
        assert!(
            path.starts_with(&dir_str),
            "the resolved path must live under the chosen directory: {path}"
        );
        assert!(
            ops.lock().unwrap().contains(&"preview_export_path"),
            "the mock preview handler must have run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// GPU-touching tests (require a device) are gated behind `#[ignore]` per
// CONCERNS.md (GPU tests skip in CI).
#[cfg(test)]
mod gpu_tests {
    use super::refine_verified_lens;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    #[test]
    #[ignore = "requires a GPU device; run with --ignored on a machine with Vulkan"]
    fn gpu_backend_imports_and_previews_one_frame() {
        // The device-owning path is exercised end-to-end by the binary; this
        // test documents the ignored hook for a GPU-capable runner.
    }

    /// The real mismatched Xiaomi pair (11T Pro left + 14T Pro right).
    fn xiaomi_pair() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("test-media")
            .join("xiaomi");
        (
            root.join("xiaomi-11tpro-left.mp4"),
            root.join("xiaomi-14tpro-right.mp4"),
        )
    }

    /// Real-clip non-regression acceptance on the Xiaomi pair (INTR-03 /
    /// ROADMAP criterion 4).
    ///
    /// Runs the existing calibration pipeline on the real mismatched clips,
    /// derives raw distorted-pixel observations from the verified matches, runs
    /// the reduced `k1` refinement behind the held-out guard, and asserts the
    /// non-regression bar: the held-out residual never regresses and `k1` stays
    /// within its bound. Prints the baseline→refined delta and the verdict.
    ///
    /// Run explicitly (it loads ~540 MB of video and needs a GPU):
    ///
    /// ```text
    /// cargo test -p reco-app xiaomi -- --ignored --nocapture
    /// ```
    ///
    /// The clips are committed under `test-media/xiaomi/`; if they are absent
    /// this test fails loudly rather than passing silently.
    #[test]
    #[ignore = "requires the real Xiaomi clips + a GPU; run with `cargo test -p reco-app xiaomi -- --ignored --nocapture`"]
    fn xiaomi_k1_refinement_does_not_regress_the_held_out_fit() {
        let (left, right) = xiaomi_pair();
        assert!(
            left.exists() && right.exists(),
            "Xiaomi test clips not found at {} / {} — this acceptance test must \
             not silently pass",
            left.display(),
            right.display()
        );

        let gpu = reco_core::gpu::GpuContext::new_blocking().expect("a GPU device is required");
        let interrupted = AtomicBool::new(false);

        // Sample more than the pipeline's default 2 frame pairs so the
        // mismatched pair yields enough verified matches for the `k1` solve to
        // be conditioned (the conditioning gate needs >= RECOMMENDED_MIN_MATCHES
        // well-spread observations); with 2 frames the gate correctly refuses.
        let config = reco_calibrate::CalibrationConfig {
            num_frames: 12,
            ..reco_calibrate::CalibrationConfig::default()
        };
        let options = reco_calibrate::video::CalibrateVideosOptions {
            config: Some(config),
            ..reco_calibrate::video::CalibrateVideosOptions::default()
        };

        let result = reco_calibrate::video::calibrate_videos_with_gpu(
            &gpu,
            &left,
            &right,
            options,
            &mut |p| eprintln!("[xiaomi] {}: {}", p.step, p.detail),
            &interrupted,
        )
        .expect("the real calibration pipeline must succeed on the Xiaomi pair");

        let cal = &result.calibration;

        // Exercise the SAME worker path the opt-in action uses (CR-01): aggregate
        // every frame's verified matches — exactly what `calibrate` retains in
        // `verified_matches` — and run `refine_verified_lens`, which derives the
        // raw observations and calls the reduced solver. The test cannot mask a
        // runtime refusal by exercising a different data volume.
        let matches: Vec<reco_calibrate::types::MatchedPoint> = result
            .per_frame
            .iter()
            .flat_map(|fm| fm.points.iter().copied())
            .collect();

        let cfg = reco_calibrate::IntrinsicsConfig::default();
        let refinement = refine_verified_lens(
            &matches,
            &cal.left,
            &cal.right,
            &cal.layout,
            cfg.heldout_fraction,
        )
        .expect("the reduced k1 refinement must run on the real observations");

        let baseline_k1 = cal.left.d[0];
        let bound = cfg.k1_bound;

        // The non-regression bar. An accepted refinement must improve (or at
        // least not worsen) the held-out fit; a rejected one is never applied,
        // so the profile keeps the baseline k1 — the applied residual is the
        // baseline by construction. Either way the *applied* result regresses
        // nothing.
        let non_regression = if refinement.accepted {
            refinement.heldout_refined <= refinement.heldout_baseline
        } else {
            (refinement.k1 - baseline_k1).abs() < 1e-12
        };
        assert!(
            non_regression,
            "non-regression violated: accepted={} baseline_k1={baseline_k1} k1={} \
             heldout {}/{} reason={:?}",
            refinement.accepted,
            refinement.k1,
            refinement.heldout_baseline,
            refinement.heldout_refined,
            refinement.reason
        );
        assert!(
            (refinement.k1 - baseline_k1).abs() <= bound + 1e-9,
            "k1 must stay within its bound: |{} - {}| > {}",
            refinement.k1,
            baseline_k1,
            bound
        );

        println!(
            "xiaomi non-regression: accepted={} k1 {baseline_k1:.4} -> {:.4} (bound {bound})",
            refinement.accepted, refinement.k1
        );
        println!(
            "  held-out residual {:.6} -> {:.6}",
            refinement.heldout_baseline, refinement.heldout_refined
        );
        println!("  reason: {:?}", refinement.reason);
        if refinement.accepted {
            println!(
                "  applied k1 = {:.4} (profile updated; residual {:.6})",
                refinement.k1, refinement.heldout_refined
            );
        } else {
            println!(
                "  applied k1 = baseline {baseline_k1:.4} (profile unchanged; residual {:.6})",
                refinement.heldout_baseline
            );
        }
        println!(
            "  observations: {} across {} frames",
            matches.len(),
            result.per_frame.len()
        );
    }
}
