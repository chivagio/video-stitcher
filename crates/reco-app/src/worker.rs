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
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
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

/// The webview readback channel type (raw frame bytes as an `ArrayBuffer`).
pub type ReadbackChannel = tauri::ipc::Channel<tauri::ipc::Response>;

/// The presenter chain handed from the setup thread to the worker (PREV-05):
/// strongest-first `(kind, pre-built presenter)` pairs.
pub type PresenterChain = Vec<(
    crate::presenter::PresenterKind,
    Box<dyn crate::presenter::SurfacePresenter + Send>,
)>;

/// The result of [`spawn_gpu_worker`]: the worker, its event receiver, and the
/// readback channel sender.
pub type SpawnedWorker = (EngineWorker, Receiver<WorkerEvent>, Sender<ReadbackChannel>);

/// A sink the worker uses to emit [`WorkerEvent`]s to the UI.
///
/// Emitting is infallible from the worker's point of view: if the UI has gone
/// away the send fails and the event is dropped. That is deliberate — the
/// worker's work should not abort because a log line could not be displayed.
pub struct EventSink {
    tx: Sender<WorkerEvent>,
}

impl EventSink {
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

/// Project a probed [`VideoProbe`](reco_io::ffmpeg::calibration_io::VideoProbe)
/// into the UI-facing [`InputMetadata`](crate::events::InputMetadata) with
/// per-field provenance (IMPT-02 / D3-04).
///
/// Pure and GPU-free so the provenance mapping is unit-testable in CI. The
/// mapping is the whole point of IMPT-02:
///
/// * `resolution` — read directly from the decoder → `Probed`.
/// * `fps` — read directly from the decoder → `Probed`.
/// * `duration` — derived from `total_frames / fps` (and `total_frames` is
///   itself a `duration × fps` estimate) → `Estimated`; never authoritative.
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

    let fps = if probe.fps > 0.0 {
        MetadataField::probed(format_fps(probe.fps))
    } else {
        MetadataField::missing(Provenance::Probed)
    };

    let duration = if probe.fps > 0.0 && probe.total_frames > 0 {
        MetadataField::estimated(format_duration(probe.total_frames as f64 / probe.fps))
    } else {
        MetadataField::missing(Provenance::Estimated)
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

    /// Report the webview chrome's collapsible state; recompute and reconfigure
    /// the native viewport (no transport/pose reset).
    fn set_chrome(&mut self, chrome: crate::presenter::ChromeState, events: &EventSink);

    /// Reconfigure the native viewport for a new window size (no transport/pose
    /// reset). Must run at a command boundary (no `SurfaceTexture` alive).
    fn resize_viewport(&mut self, width: u32, height: u32, events: &EventSink);

    /// Run the hardcoded file→file export until `interrupted` is set.
    fn export(&mut self, events: &EventSink, interrupted: &AtomicBool) -> Result<(), WorkerError>;

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
        } => {
            backend.set_chrome(
                crate::presenter::ChromeState {
                    panel_expanded,
                    drawer_expanded,
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
        // The calibration vocabulary lands in Task 3 of this plan; until the
        // backend methods exist these commands are rejected with a typed error
        // rather than silently dropped (they are never reachable from the UI
        // before the wizard ships). Task 3 replaces each arm with the real call.
        WorkerCommand::ClearInput { .. }
        | WorkerCommand::LensCandidates { .. }
        | WorkerCommand::SetLensOverride { .. }
        | WorkerCommand::ClearLensOverride { .. }
        | WorkerCommand::StartCalibration { .. }
        | WorkerCommand::LoadProfile { .. }
        | WorkerCommand::SaveProfile { .. } => {
            events.failed(WorkerError::Unsupported {
                operation: "calibration commands are not wired yet".to_string(),
            });
        }
        WorkerCommand::Export => {
            interrupted.store(false, Ordering::SeqCst);
            match backend.export(events, interrupted) {
                Ok(()) => events.info("export finished"),
                Err(e) => events.failed(e),
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
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    let is_job = matches!(
                        cmd,
                        WorkerCommand::Import
                            | WorkerCommand::Preview
                            | WorkerCommand::Export
                            | WorkerCommand::SetInput { .. }
                    );
                    let keep_going = handle_command(cmd, &mut backend, &events, &interrupted);
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

        match rx.recv() {
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
    pub fn spawn<B: EngineBackend + 'static>(backend: B) -> (Self, Receiver<WorkerEvent>) {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        let events = EventSink { tx: evt_tx };
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
    /// The loaded calibration, if `Import` has run.
    calibration: Option<reco_core::calibration::MatchCalibration>,
    /// The open decode source, if `Import` has run.
    ///
    /// Drops **first** (declaration order): the CLI documents that the decode
    /// source (NVDEC/CUDA) must drop before the wgpu renderer/device to avoid a
    /// CUDA-context teardown race (`crates/reco-cli/src/preview.rs:213-215`,
    /// RESEARCH Pitfall 5).
    source: Option<reco_io::adapters::FfmpegFileSource>,
    /// Input frame dimensions from the source metadata.
    input_size: Option<(u32, u32)>,
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
    /// The single GPU device owner (FOUND-03). Declared **last** so it drops
    /// after the decode source, renderer, and presenter surface — the documented
    /// teardown order (FOUND-06 / RESEARCH Pattern 6). Actually field 1 held the
    /// `GpuContext` in the original skeleton; it now lives here, last.
    gpu: reco_core::gpu::GpuContext,
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
            calibration: None,
            source: None,
            input_size: None,
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
        self.input_paths.insert(role, path);
        events.info(format!("{} selected: {filename}", role.label()));
        events.metadata(role, metadata);
        Ok(())
    }

    fn begin_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
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
        self.reconfigure_viewport(events);
    }

    fn resize_viewport(&mut self, width: u32, height: u32, events: &EventSink) {
        self.window_width = width;
        self.window_height = height;
        self.reconfigure_viewport(events);
    }

    fn export(&mut self, events: &EventSink, interrupted: &AtomicBool) -> Result<(), WorkerError> {
        let paths =
            crate::hardcoded::media_paths().map_err(|e| WorkerError::Engine(e.to_string()))?;
        let output = crate::hardcoded::export_output_path()
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        events.info("export started");
        reco_io::StitchJob::new(
            paths.left.as_path(),
            paths.right.as_path(),
            paths.calibration.as_path(),
            output.as_path(),
        )
        .run(interrupted)
        .map_err(|e| WorkerError::Engine(e.to_string()))?;
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
/// Returns the worker, the event receiver, and the **readback channel sender**
/// (PREV-05): the Tauri command layer hands a webview `Channel<Response>` to the
/// worker through it, which then attaches it to the readback presenter.
///
/// # Errors
///
/// [`WorkerError::Engine`] if device creation or surface configuration fails.
pub fn spawn_gpu_worker(
    instance: reco_core::wgpu::Instance,
    presenters: PresenterChain,
    viewport: crate::presenter::ViewportRect,
    startup_fallback: Option<String>,
    calibration_cancel: Arc<AtomicBool>,
) -> Result<SpawnedWorker, WorkerError> {
    let backend =
        GpuEngineBackend::new(instance, presenters, viewport, startup_fallback, calibration_cancel)?;
    let readback_tx = backend.readback_sender();
    let (worker, events) = EngineWorker::spawn(backend);
    Ok((worker, events, readback_tx))
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
    }

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

        fn record(&self, op: &'static str) {
            self.ops.lock().unwrap().push(op);
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
            let filename = std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str())
                .to_string();
            events.info(format!("{} selected: {filename}", role.label()));
            // Mirror the real backend's typed event shape so a worker-loop test
            // can assert the protocol without a GPU.
            events.metadata(
                role,
                crate::events::InputMetadata {
                    resolution: crate::events::MetadataField::probed("1920×1080"),
                    fps: crate::events::MetadataField::probed("30 fps"),
                    duration: crate::events::MetadataField::estimated("0:02"),
                    codec: crate::events::MetadataField::missing(crate::events::Provenance::Probed),
                },
            );
            Ok(())
        }

        fn begin_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("begin_preview");
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

        fn set_chrome(&mut self, _chrome: crate::presenter::ChromeState, _events: &EventSink) {
            self.record("set_chrome");
        }

        fn resize_viewport(&mut self, _width: u32, _height: u32, _events: &EventSink) {
            self.record("resize_viewport");
        }

        fn export(
            &mut self,
            _events: &EventSink,
            interrupted: &AtomicBool,
        ) -> Result<(), WorkerError> {
            self.record("export");
            interrupted.store(true, Ordering::SeqCst);
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
    fn project_metadata_reports_probed_codec_and_derived_duration_estimated() {
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
        assert_eq!(md.fps.value.as_deref(), Some("30 fps"));
        assert_eq!(md.fps.provenance, Provenance::Probed);
        // 60 frames / 30 fps = 2 s -> "0:02", and it is DERIVED (estimated).
        assert_eq!(md.duration.value.as_deref(), Some("0:02"));
        assert_eq!(md.duration.provenance, Provenance::Estimated);
        // E2: a container codec is a direct read -> probed, never absent.
        assert_eq!(md.codec.value.as_deref(), Some("h264"));
        assert_eq!(md.codec.provenance, Provenance::Probed);
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
            },
        );
        let line = native_geometry_line(rect, Some((1000, 728)), rect_chrome_expanded());
        assert!(line.contains("requested 1000x728"), "{line}");
        assert!(line.contains("child window 1000x728"), "{line}");
        assert!(line.contains("panel expanded"), "{line}");
        assert!(line.contains("drawer collapsed"), "{line}");
        assert!(!line.contains("mismatch"), "{line}");
    }

    /// The chrome state that produces a 1000x728 rect at 1280x800.
    fn rect_chrome_expanded() -> crate::presenter::ChromeState {
        crate::presenter::ChromeState {
            panel_expanded: true,
            drawer_expanded: false,
        }
    }

    #[test]
    fn native_geometry_line_names_the_mismatch_instead_of_hiding_it() {
        // A window that kept its old size must be visible in the log, because
        // this is exactly the UAT gap: the surface was reconfigured correctly
        // and the OS window was not, and nothing inside Rust could tell.
        let rect = crate::presenter::ViewportRect::for_chrome(1280, 800, &rect_chrome_expanded());
        let line = native_geometry_line(rect, Some((1240, 728)), rect_chrome_expanded());
        assert!(line.contains("requested 1000x728"), "{line}");
        assert!(line.contains("child window 1240x728"), "{line}");
        assert!(line.contains("mismatch"), "{line}");
    }

    #[test]
    fn native_geometry_line_says_unavailable_rather_than_reporting_success() {
        // A presenter with no window of its own (readback/fallback) must not
        // read as though the request was achieved.
        let rect = crate::presenter::ViewportRect::for_chrome(1280, 800, &Default::default());
        let line = native_geometry_line(rect, None, Default::default());
        assert!(line.contains("requested 1240x728"), "{line}");
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
        let events = EventSink { tx: evt_tx };
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
        let events = EventSink { tx: evt_tx };
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
        let events = EventSink { tx: evt_tx };
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
        handle.send(WorkerCommand::Export).unwrap();
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
        assert_eq!(
            &*ops.lock().unwrap(),
            &["set_chrome", "resize_viewport", "shutdown"]
        );
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
}

// GPU-touching tests (require a device) are gated behind `#[ignore]` per
// CONCERNS.md (GPU tests skip in CI).
#[cfg(test)]
mod gpu_tests {
    #[test]
    #[ignore = "requires a GPU device; run with --ignored on a machine with Vulkan"]
    fn gpu_backend_imports_and_previews_one_frame() {
        // The device-owning path is exercised end-to-end by the binary; this
        // test documents the ignored hook for a GPU-capable runner.
    }
}
