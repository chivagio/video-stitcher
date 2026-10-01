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

    /// Run the live stitched-frame loop until `interrupted` is set or the
    /// source is exhausted.
    fn preview(&mut self, events: &EventSink, interrupted: &AtomicBool) -> Result<(), WorkerError>;

    /// Run the hardcoded file→file export until `interrupted` is set.
    fn export(&mut self, events: &EventSink, interrupted: &AtomicBool) -> Result<(), WorkerError>;

    /// Dispatch a transport-agnostic input intent to the worker's pose state.
    fn dispatch_intent(&mut self, intent: reco_control::ControlIntent);

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
            interrupted.store(false, Ordering::SeqCst);
            match backend.preview(events, interrupted) {
                Ok(()) => events.info("preview stopped"),
                Err(e) => events.failed(e),
            }
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

/// The worker loop: drain pending commands (FIFO), then block when idle.
///
/// Generic over the backend and driven only by a receiver + an event sink, so
/// it is unit-testable with a mock backend and no GPU.
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
        mut presenter: Box<dyn crate::presenter::SurfacePresenter + Send>,
        viewport: crate::presenter::ViewportRect,
    ) -> Result<Self, WorkerError> {
        // Adapter-retaining `for_surface` path: the presenter needs the adapter
        // for `get_capabilities`, and the worker needs the device. Never a
        // second device (D-03).
        let surface = presenter
            .surface()
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let (gpu, surface_info) =
            pollster::block_on(reco_core::gpu::GpuContext::for_surface(&instance, surface))
                .map_err(|e| WorkerError::Engine(e.to_string()))?;

        let adapter = gpu
            .adapter()
            .ok_or_else(|| WorkerError::Engine("engine did not retain a wgpu::Adapter".into()))?
            .clone();

        // FOUND-05: register the loss/uncaptured-error handlers at device init,
        // *before* the first command can run. The callback only sets a flag —
        // recovery happens on the worker thread in `recover`.
        let device_lost = Arc::new(AtomicBool::new(false));
        install_device_lost_handlers(gpu.device(), Arc::clone(&device_lost));

        presenter
            .configure(
                gpu.device(),
                gpu.queue(),
                &adapter,
                viewport.width,
                viewport.height,
            )
            .map_err(|e| WorkerError::Engine(e.to_string()))?;

        Ok(Self {
            gpu,
            surface_format: surface_info.format,
            pose: reco_control::pose_control::PoseControl::with_defaults(),
            viewport,
            blend_width: 0.05,
            rig_tilt: 0.0,
            calibration: None,
            source: None,
            input_size: None,
            presenter,
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
                self.viewport.width,
                self.viewport.height,
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
        let surface = self
            .presenter
            .surface()
            .map_err(|e| WorkerError::Engine(e.to_string()))?;
        let (gpu, surface_info) = pollster::block_on(reco_core::gpu::GpuContext::for_surface(
            &self.instance,
            surface,
        ))
        .map_err(|e| WorkerError::Engine(e.to_string()))?;
        self.adapter = gpu
            .adapter()
            .ok_or_else(|| WorkerError::Engine("rebuilt device retained no adapter".into()))?
            .clone();
        self.gpu = gpu;
        self.surface_format = surface_info.format;
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
pub fn dispatch_intent(
    pose: &mut reco_control::pose_control::PoseControl,
    intent: reco_control::ControlIntent,
) {
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

    fn preview(&mut self, events: &EventSink, interrupted: &AtomicBool) -> Result<(), WorkerError> {
        // Clone (not `take`) the calibration: the renderer takes it by value,
        // but a preview must be repeatable without re-importing, so the loaded
        // calibration stays owned by the backend (Plan 04 re-runs preview from
        // the UI). `MatchCalibration` is `Clone`.
        let (cal, (in_w, in_h)) = match (self.calibration.clone(), self.input_size) {
            (Some(cal), Some(size)) => (cal, size),
            _ => return Err(WorkerError::NotImported),
        };

        // Build the renderer if recovery has not already done so, and cache the
        // inputs so a device loss during preview can rebuild it without
        // re-importing (FOUND-05).
        if self.renderer.is_none() {
            self.renderer = Some(self.build_renderer(cal.clone(), in_w, in_h)?);
        }
        self.renderer_input = Some((cal, in_w, in_h));

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

        events.info("preview started");
        let mut frames: u64 = 0;
        // Frame loop: decode → render into the presenter's surface → present.
        // The loop checks `interrupted` every frame so a `Shutdown` queued by
        // the drain-at-top-of-loop is honored promptly (D-07).
        while !interrupted.load(Ordering::SeqCst) {
            // A device loss flagged mid-preview is recovered here, between
            // frames (never while a SurfaceTexture is alive — we are before the
            // acquire).
            if self.recovery_pending() {
                self.recover(events)?;
            }
            let pair = match self
                .source
                .as_mut()
                .ok_or(WorkerError::NotImported)?
                .next_frame()
                .map_err(|e| WorkerError::Engine(e.to_string()))?
            {
                Some(reco_core::source::StereoFrame::Yuv420p(pair)) => pair,
                Some(_) => continue,
                None => {
                    events.info("preview reached end of source");
                    break;
                }
            };

            let render = self.pose.render_pose(self.rig_tilt);
            // Compute the outcome first so the `renderer` borrow (of
            // `self.renderer`) ends before any arm takes `&mut self`
            // (recovery / device-loss simulation).
            let outcome = {
                let renderer = self
                    .renderer
                    .as_ref()
                    .ok_or_else(|| WorkerError::Engine("renderer missing during preview".into()))?;
                self.presenter.render_frame(
                    renderer,
                    &pair.left,
                    &pair.right,
                    render.yaw,
                    render.pitch,
                )
            };
            match outcome {
                Ok(crate::presenter::FrameOutcome::Presented) => {
                    frames += 1;
                    if frames == 1 {
                        // The A1 marker: reaching this line proves the worker
                        // decoded a real frame pair, rendered through the shared
                        // device, and presented it into the native child view
                        // (Plan 05's gate report keys on this).
                        events.info(
                            "A1 verdict: engine worker presented a stitched frame \
                             into the native child view",
                        );
                        // FOUND-05 debug affordance: when enabled, force a
                        // device loss right after the first real frame so a
                        // gate run can observe recovery. Off unless the env var
                        // is set; never wired to the UI (UI-SPEC rule 4).
                        if crate::hardcoded::debug_device_loss_enabled() {
                            events.info("debug device-loss simulation enabled — destroying device");
                            self.simulate_device_loss(events)?;
                            // Recover explicitly here rather than relying on the
                            // loop's next recovery check: the simulation may be
                            // the last thing that happens before the worker blocks
                            // on `recv()`, in which case the device-lost flag
                            // would stay set and recovery would never be observed
                            // (nor the device rebuilt). `recover` is a no-op when
                            // the flag is clear, and `destroy()` guarantees it is
                            // set by the time this returns.
                            self.device_lost.store(true, Ordering::SeqCst);
                            self.recover(events)?;
                        }
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
                        "presenter is not configured during preview".to_string(),
                    ));
                }
                Err(e) => {
                    // Other render failure: surface the typed error (do not
                    // present a black frame, do not panic).
                    return Err(WorkerError::Engine(e.to_string()));
                }
            }
        }
        if interrupted.load(Ordering::SeqCst) {
            events.info("preview interrupted");
        } else {
            events.info(format!("preview presented {frames} frame(s)"));
        }
        Ok(())
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
/// presenter (which owns the render target).
///
/// # Errors
///
/// [`WorkerError::Engine`] if device creation or surface configuration fails.
pub fn spawn_gpu_worker(
    instance: reco_core::wgpu::Instance,
    presenter: Box<dyn crate::presenter::SurfacePresenter + Send>,
    viewport: crate::presenter::ViewportRect,
) -> Result<(EngineWorker, Receiver<WorkerEvent>), WorkerError> {
    let backend = GpuEngineBackend::new(instance, presenter, viewport)?;
    Ok(EngineWorker::spawn(backend))
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
    struct MockBackend {
        ops: Arc<std::sync::Mutex<Vec<&'static str>>>,
        pose: Arc<std::sync::Mutex<reco_control::pose_control::PoseControl>>,
        import_fails: bool,
        /// Mirrors the real backend's device-lost flag (FOUND-05).
        lost: Arc<AtomicBool>,
    }

    impl MockBackend {
        fn new(ops: Arc<std::sync::Mutex<Vec<&'static str>>>) -> Self {
            Self {
                ops,
                pose: Arc::new(std::sync::Mutex::new(
                    reco_control::pose_control::PoseControl::with_defaults(),
                )),
                import_fails: false,
                lost: Arc::new(AtomicBool::new(false)),
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

        fn preview(
            &mut self,
            _events: &EventSink,
            interrupted: &AtomicBool,
        ) -> Result<(), WorkerError> {
            self.record("preview");
            interrupted.store(true, Ordering::SeqCst);
            Ok(())
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
