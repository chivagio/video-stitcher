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
//! lifetime, and drops them in a fixed order at shutdown:
//!
//! ```text
//!   decode source  →  StitchRenderer  →  GpuContext (device)  →  presenter Surface
//!   (drop first,        (then)             (then)                 (last: window outlives surface)
//!    NVDEC/CUDA race)
//! ```
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
            // Reserved for Plan 05 (FOUND-05). Rejected as a typed error so the
            // protocol shape is exercised until the handler is implemented.
            events.failed(WorkerError::Unsupported {
                operation: "simulate_device_loss".to_string(),
            });
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
    // FOUND-06 teardown; wired by the app-close handler in Plan 05 and
    // exercised by this module's idle-worker test.
    #[cfg_attr(not(test), allow(dead_code))]
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
// See [`EngineWorker::shutdown`] — FOUND-06 teardown, test-exercised.
#[cfg_attr(not(test), allow(dead_code))]
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
    /// One-time GPU context: the single device owner (FOUND-03).
    gpu: reco_core::gpu::GpuContext,
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
    /// The open decode source, if `Import` has run (drops first — see above).
    source: Option<reco_io::adapters::FfmpegFileSource>,
    /// Input frame dimensions from the source metadata.
    input_size: Option<(u32, u32)>,
    /// The render target the worker draws into (D-03). The worker owns the
    /// presenter for the app's lifetime; the UI never touches it.
    presenter: Box<dyn crate::presenter::SurfacePresenter + Send>,
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
            .ok_or_else(|| WorkerError::Engine("engine did not retain a wgpu::Adapter".into()))?;
        presenter
            .configure(gpu.device(), adapter, viewport.width, viewport.height)
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
        })
    }
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
        let (cal, (in_w, in_h)) = match (self.calibration.take(), self.input_size) {
            (Some(cal), Some(size)) => (cal, size),
            _ => return Err(WorkerError::NotImported),
        };
        // Borrow the source separately so the renderer (which clones the device)
        // and the decode loop do not conflict.
        let source = self.source.as_mut().ok_or(WorkerError::NotImported)?;

        let viewport =
            crate::presenter::viewport_config(self.viewport, self.blend_width, self.rig_tilt);
        let renderer = reco_core::render::stitch_renderer::StitchRenderer::new(
            cal,
            self.gpu.clone(),
            viewport,
            in_w,
            in_h,
            self.surface_format,
            reco_core::render::renderer::InputFormat::Yuv420p,
        )
        .map_err(|e| WorkerError::Engine(e.to_string()))?;

        events.info("preview started");
        let mut frames: u64 = 0;
        // Frame loop: decode → render into the presenter's surface → present.
        // The loop checks `interrupted` every frame so a `Shutdown` queued by
        // the drain-at-top-of-loop is honored promptly (D-07).
        while !interrupted.load(Ordering::SeqCst) {
            match source
                .next_frame()
                .map_err(|e| WorkerError::Engine(e.to_string()))?
            {
                Some(reco_core::source::StereoFrame::Yuv420p(pair)) => {
                    let render = self.pose.render_pose(self.rig_tilt);
                    self.presenter
                        .render_frame(&renderer, &pair.left, &pair.right, render.yaw, render.pitch)
                        .map_err(|e| WorkerError::Engine(e.to_string()))?;
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
                    }
                }
                Some(_) => continue,
                None => {
                    events.info("preview reached end of source");
                    break;
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

    fn shutdown(&mut self) {
        // FOUND-06 drop order: release the decode source before the GPU
        // context (the field order enforces the rest).
        self.source = None;
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
    }

    impl MockBackend {
        fn new(ops: Arc<std::sync::Mutex<Vec<&'static str>>>) -> Self {
            Self {
                ops,
                pose: Arc::new(std::sync::Mutex::new(
                    reco_control::pose_control::PoseControl::with_defaults(),
                )),
                import_fails: false,
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
    fn simulate_device_loss_is_rejected_as_unsupported() {
        let ops = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (worker, events) = EngineWorker::spawn(MockBackend::new(Arc::clone(&ops)));
        worker
            .handle()
            .send(WorkerCommand::SimulateDeviceLoss)
            .unwrap();
        worker.handle().send(WorkerCommand::Shutdown).unwrap();

        let seen = drain_until_shutdown(&events);
        let failed = seen.iter().find_map(|e| match e {
            WorkerEvent::Failed(err) => Some(err.clone()),
            _ => None,
        });
        assert_eq!(
            failed,
            Some(WorkerError::Unsupported {
                operation: "simulate_device_loss".to_string()
            })
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
