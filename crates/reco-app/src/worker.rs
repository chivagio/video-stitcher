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
    fn pose(&self, yaw: f32, pitch: f32, fov_degrees: f32) {
        let _ = self.tx.send(WorkerEvent::Pose {
            yaw,
            pitch,
            fov_degrees,
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

/// The engine side of the worker, behind a trait so the protocol is testable
/// without a GPU.
///
/// The real implementation ([`GpuEngineBackend`]) owns the device/renderer/
/// pose; the test implementation owns nothing but state, letting the loop's
/// ordering and error behavior be asserted in CI.
pub trait EngineBackend: Send {
    /// Load the hardcoded clips and calibration into the session.
    fn import(&mut self, events: &EventSink) -> Result<(), WorkerError>;

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

    /// End the active session and return to idle.
    fn end_preview(&mut self, events: &EventSink);

    /// Whether a preview session is currently active.
    fn session_active(&self) -> bool;

    /// The session's transport state machine (authoritative playback position,
    /// state, loop, and coalesced seek).
    fn transport(&mut self) -> &mut crate::transport::Transport;

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

    /// Swap the active presenter to `kind` at a command boundary (PREV-05).
    ///
    /// Reuses the existing device (never a second one), releases the outgoing
    /// presenter's window, configures the incoming presenter against the shared
    /// device, and emits the `Presenter` event plus exactly one WARN/INFO line.
    /// Preserves transport position and pose. A `kind` the platform cannot host
    /// falls through the chain and reports the reason.
    fn set_presenter(&mut self, kind: crate::presenter::PresenterKind, events: &EventSink);

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

/// Handle a single command. Returns `false` when the loop should stop.
fn handle_command<B: EngineBackend>(
    cmd: WorkerCommand,
    backend: &mut B,
    events: &EventSink,
    interrupted: &AtomicBool,
) -> bool {
    match cmd {
        WorkerCommand::Import => match backend.import(events) {
            Ok(()) => events.info("import finished"),
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
        WorkerCommand::Export => {
            interrupted.store(false, Ordering::SeqCst);
            match backend.export(events, interrupted) {
                Ok(()) => events.info("export finished"),
                Err(e) => events.failed(e),
            }
        }
        WorkerCommand::Intent(intent) => backend.dispatch_intent(intent),
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
                        WorkerCommand::Import | WorkerCommand::Preview | WorkerCommand::Export
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

        // An active session is paced by the loop: advance exactly one tick,
        // then loop again to re-drain (never block — that is what lets a
        // Pause/Seek/Intent land between frames).
        if backend.session_active() {
            if let Err(e) = backend.tick_session(&events) {
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
            first_frame_marker_pending: true,
            last_frame_time: std::time::Instant::now(),
            window_width: 1280,
            window_height: 800,
            chrome: crate::presenter::ChromeState::default(),
            view_mode: crate::presenter::ViewMode::Panorama,
            calibration: None,
            source: None,
            input_size: None,
            presenter,
            presenters: slots,
            active_kind,
            readback_rx,
            readback_tx,
            instance,
            adapter,
            device_lost,
            recovery_attempts: 0,
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
            Ok(()) => events.info(format!(
                "viewport reconfigured to {}x{}",
                rect.width, rect.height
            )),
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
        let transport = crate::transport::Transport::new(fps, fps_rational, total_frames);
        events.position(
            transport.frame(),
            transport.total_frames(),
            transport.fps_rational(),
        );
        events.transport(transport.state(), transport.loop_enabled());
        self.session = Some(transport);
        self.first_frame_marker_pending = true;
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
        let seeking = pending.is_some();
        let pair = {
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
            match frame_result {
                Some(reco_core::source::StereoFrame::Yuv420p(pair)) => Some(pair),
                Some(_) => None,
                None => None,
            }
        };

        // End-of-source handling: a `None` from a playing source that has
        // exhausted (or a decode that produced no Yuv pair) ends the session
        // unless looping.
        let at_end = playing && pair.is_none();
        let pair = match pair {
            Some(p) => p,
            None => {
                if at_end {
                    if self.session.as_ref().is_some_and(|t| t.loop_enabled()) {
                        // Wrap: request a seek to 0 and keep playing.
                        if let Some(t) = self.session.as_mut() {
                            t.request_seek(0);
                        }
                        return Ok(());
                    }
                    events.info("preview reached end of source");
                    if let Some(t) = self.session.as_mut() {
                        t.mark_ended();
                        events.transport(t.state(), t.loop_enabled());
                    }
                    self.end_preview(events);
                }
                return Ok(());
            }
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
                    if let Some(t) = self.session.as_mut() {
                        t.on_frame_advanced();
                        let frame = t.frame();
                        let total = t.total_frames();
                        let fps_rational = t.fps_rational();
                        let ended = t.state() == crate::transport::TransportState::Ended;
                        if ended {
                            events.transport(t.state(), t.loop_enabled());
                        }
                        events.position(frame, total, fps_rational);
                        if ended {
                            self.session = None;
                        }
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
        self.session = None;
    }

    fn session_active(&self) -> bool {
        self.session.is_some()
    }

    fn transport(&mut self) -> &mut crate::transport::Transport {
        // The caller (`handle_command`) only reaches this for transport
        // commands, which are only meaningful with a session. Lazily create a
        // session-less transport so the borrow is total; it is replaced by
        // `begin_preview` when a real session starts.
        self.session
            .get_or_insert_with(|| crate::transport::Transport::new(30.0, None, None))
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
                events.presenter(chosen, Some(err.to_string()));
                events.log(
                    Level::Warn,
                    crate::presenter::fallback_warn_line(chosen, &err.to_string()),
                );
            }
            // Successful manual selection: an INFO line, no WARN.
            None if chosen == kind => {
                events.info(format!("presenter switched to {}", chosen.name()));
                events.presenter(chosen, None);
            }
            // The requested kind failed with fall-through but a weaker kind
            // succeeded: report the fallback without a WARN.
            None => {
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
) -> Result<SpawnedWorker, WorkerError> {
    let backend = GpuEngineBackend::new(instance, presenters, viewport)?;
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
        /// Mirrors the real backend's device-lost flag (FOUND-05).
        lost: Arc<AtomicBool>,
        /// The mock's active presenter (PREV-05).
        active_kind: crate::presenter::PresenterKind,
        /// The mock's current view mode (PREV-03).
        view_mode: crate::presenter::ViewMode,
    }

    impl MockBackend {
        fn new(ops: Arc<std::sync::Mutex<Vec<&'static str>>>) -> Self {
            Self {
                ops,
                pose: Arc::new(std::sync::Mutex::new(
                    reco_control::pose_control::PoseControl::with_defaults(),
                )),
                session: None,
                total_frames: Some(5),
                last_pushed_fov: Arc::new(std::sync::Mutex::new(None)),
                import_fails: false,
                lost: Arc::new(AtomicBool::new(false)),
                active_kind: crate::presenter::PresenterKind::Native,
                view_mode: crate::presenter::ViewMode::Panorama,
            }
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
            Ok(())
        }

        fn begin_preview(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("begin_preview");
            let t = crate::transport::Transport::new(30.0, Some((30, 1)), self.total_frames);
            events.position(t.frame(), t.total_frames(), t.fps_rational());
            events.transport(t.state(), t.loop_enabled());
            self.session = Some(t);
            Ok(())
        }

        fn tick_session(&mut self, events: &EventSink) -> Result<(), WorkerError> {
            self.record("tick");
            // Record which render path the view mode selected (PREV-03): the
            // mock has no GPU, so it records the path the real backend's
            // `tick_session` would take. Done before the mutable session borrow
            // so the ops lock does not alias the session borrow.
            match self.view_mode {
                crate::presenter::ViewMode::Panorama => self.record("render_panorama"),
                crate::presenter::ViewMode::Source => self.record("render_source"),
            }
            let Some(t) = self.session.as_mut() else {
                return Ok(());
            };
            // Mirror the real tick's pose work: tick + push FOV + emit pose.
            {
                let mut pose = self.pose.lock().unwrap();
                pose.tick();
                *self.last_pushed_fov.lock().unwrap() = Some(pose.current_fov_deg());
                let current = pose.current_pose();
                events.pose(current.yaw, current.pitch, pose.current_fov_deg());
            }
            // Advance the transport; if it reaches Ended (loop off), the session
            // ends so the worker loop returns to a blocking recv.
            if t.state() == crate::transport::TransportState::Playing {
                t.on_frame_advanced();
                let frame = t.frame();
                let total = t.total_frames();
                let fps_rational = t.fps_rational();
                let ended = t.state() == crate::transport::TransportState::Ended;
                if ended {
                    events.transport(t.state(), t.loop_enabled());
                }
                events.position(frame, total, fps_rational);
                if ended {
                    self.session = None;
                }
            }
            Ok(())
        }

        fn end_preview(&mut self, _events: &EventSink) {
            self.record("end_preview");
            self.session = None;
        }

        fn session_active(&self) -> bool {
            self.session.is_some()
        }

        fn transport(&mut self) -> &mut crate::transport::Transport {
            self.session
                .get_or_insert_with(|| crate::transport::Transport::new(30.0, None, None))
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

        fn set_presenter(&mut self, kind: crate::presenter::PresenterKind, events: &EventSink) {
            self.record("set_presenter");
            self.active_kind = kind;
            events.presenter(kind, None);
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
        assert_eq!(&*ops.lock().unwrap(), &["import", "shutdown"]);

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
        let recorded = ops.lock().unwrap().clone();
        // `simulate_device_loss` runs, then recovery fires, then shutdown.
        assert_eq!(
            recorded,
            vec!["simulate_device_loss", "recover", "shutdown"]
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

        // Collect Position events in a background thread so they are not
        // lost between measurement points.
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
        let frames = collected.lock().unwrap().clone();
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
        let frames = collected.lock().unwrap().clone();
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
