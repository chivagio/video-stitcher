//! `reco-app` — Tauri 2 desktop host that links the Reco engine crates
//! in-process and composites a native `wgpu::Surface` under the Tauri webview.
//!
//! # Phase 1 walking skeleton (FOUND-01, FOUND-02)
//!
//! This is the phase's existential risk step: prove on ONE target first
//! (Linux/X11) that a Rust-owned native **child view** under the Tauri webview
//! can host a `wgpu::Surface` on the **shared** engine device, render one real
//! stitched frame from two hardcoded clips, and present it composited under the
//! webview (CONTEXT D-01/D-02/D-03; RESEARCH assumptions A1/A3).
//!
//! ```text
//!   Tauri/tao window
//!   ├── chrome webview (bottom 208px: log 160 + control row 48)
//!   └── native X11 child view (top region) ── owns wgpu::Surface (shared device)
//! ```
//!
//! Window and child-view creation happen in `tauri::Builder::setup`, never in a
//! synchronous Tauri command (Windows deadlock, wry #583 — RESEARCH Pitfall 2).
//!
//! This binary is the only place `anyhow` is used; the presenter/engine types
//! use typed `thiserror` errors (CONVENTIONS.md).

mod commands;
mod events;
mod hardcoded;
mod presenter;
mod worker;

#[cfg(all(unix, not(target_os = "macos")))]
use presenter::x11::X11Presenter;
use presenter::{PresenterError, ViewportRect};
#[cfg(all(unix, not(target_os = "macos")))]
use tauri::Manager as _;

/// Install the standard tracing subscriber + log bridge.
///
/// The binary installs the subscriber once; libraries must not
/// (CONVENTIONS.md). Mirrors `crates/reco-gui/src/main.rs:1223`.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let _ = tracing_log::LogTracer::init();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ort::logging=warn"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .try_init();
}

fn main() -> anyhow::Result<()> {
    init_tracing();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::import,
            commands::preview,
            commands::export
        ])
        .setup(|app| {
            if let Err(e) = run_skeleton(app) {
                // Surface the exact A1/A3 outcome rather than masking it: this
                // is the walking-skeleton gate and a failure is a recorded
                // verdict, not a silent no-op.
                log::error!("reco-app skeleton failed: {e}");
                eprintln!("reco-app skeleton FAILED: {e}");
            }
            Ok(())
        })
        .run(tauri::generate_context!())?;

    Ok(())
}

/// Create the window, the presenter's shared device, and start the engine
/// worker that exclusively owns the device (FOUND-03 / D-06).
///
/// Returns a typed error so the exact A1/A3 failure is visible (the caller logs
/// it; the gate report (Plan 05) folds it in).
#[cfg(all(unix, not(target_os = "macos")))]
fn run_skeleton(app: &mut tauri::App) -> Result<(), SkeletonError> {
    let (window, rect) = build_window_and_chrome(app)?;

    // Build the presenter first: the presenter owns its Surface (D-03), and
    // that surface drives adapter selection when the worker creates the device.
    let instance = reco_core::wgpu::Instance::default();
    let presenter = X11Presenter::new(&window, &instance, rect)?;

    // Ownership handoff (FOUND-03): the device is created *inside* the worker
    // from the presenter's surface, so the worker is the sole device owner.
    // Nothing on this (the setup) thread holds a device handle or renders
    // directly.
    let (worker, events) = worker::spawn_gpu_worker(instance, Box::new(presenter), rect)?;

    // The webview bridge: drain typed worker events on an async Tauri task and
    // forward each one to the frontend. The JS `listen("worker-event")` side
    // renders them (Plan 04).
    install_event_bridge(app.handle().clone(), events);

    // The managed `WorkerHandle` is what `State<WorkerHandle>` resolves in the
    // command handlers. Capture it before moving the worker into teardown.
    let handle = worker.handle();

    // FOUND-06: register the clean-teardown path on the window. A close request
    // must stop the worker, join it within a timeout, and drop the engine in the
    // documented order — never hang, never `std::process::exit`.
    install_close_handler(&window, worker);

    // The thin path is driven by the WEBVIEW, not from here: the three
    // `#[tauri::command]` handlers post Import / Preview / Export when the user
    // presses the matching button (D-06/D-09). We deliberately do NOT auto-post
    // Import/Preview at startup — the whole point of Plan 04's UI is that the
    // user drives the engine without a CLI, and the busy/disabled interaction
    // contract proves the single-owner boundary as each command is issued.

    // Keep the window (whose child the worker renders into) alive for the app's
    // lifetime; the webview owns the lifetime from here.
    app.manage(handle);
    app.manage(window);

    Ok(())
}

/// FOUND-06 teardown state: owns the worker's `JoinHandle` until close.
///
/// The app lives for the webview's lifetime; this state exists so the close
/// handler can take ownership of the worker and join it. Held in a `Mutex` (the
/// only lock in `reco-app`, and it is never held across a render tick — it only
/// guards the one-shot close path, so it cannot poison a hot path).
#[cfg(all(unix, not(target_os = "macos")))]
struct TeardownState {
    worker: std::sync::Mutex<Option<worker::EngineWorker>>,
}

/// How long to wait for the worker to stop before giving up (FOUND-06).
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Register the window-close handler that performs clean teardown (FOUND-06).
///
/// On [`tauri::WindowEvent::CloseRequested`] / `Destroyed`, the worker is asked
/// to stop and joined within [`SHUTDOWN_TIMEOUT`]. The drop order is enforced by
/// the worker's field order and `shutdown()` (stop loop → drop decode → drop
/// renderer → drop surface → drop device) — see `worker.rs` for the NVDEC/wgpu
/// hazard citation. The event bridge task ends when the worker drops its event
/// sender, so the process can exit.
#[cfg(all(unix, not(target_os = "macos")))]
fn install_close_handler(window: &tauri::window::Window, worker: worker::EngineWorker) {
    let state = TeardownState {
        worker: std::sync::Mutex::new(Some(worker)),
    };
    // The state is moved into the handler and returned to Tauri via `manage` so
    // it is not dropped before the event fires.
    let shared = std::sync::Arc::new(state);
    window.app_handle().manage(shared.clone());

    window.on_window_event(move |event| {
        if matches!(
            event,
            tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
        ) {
            // Take the worker once; a second close event is a no-op.
            let taken = shared.worker.lock().ok().and_then(|mut guard| guard.take());
            if let Some(worker) = taken {
                match worker.shutdown(SHUTDOWN_TIMEOUT) {
                    Ok(()) => log::info!("engine worker stopped cleanly"),
                    Err(e) => log::warn!("engine worker shutdown: {e}"),
                }
            }
        }
    });
}

/// Forward typed [`worker::WorkerEvent`]s to the webview on an async task.
///
/// Drains the worker's event channel on Tauri's async runtime and emits each
/// event, projected to the UI-facing [`events::LogLine`] shape, under the
/// `"worker-event"` event name. The frontend (`ui/main.ts`) listens with
/// `listen("worker-event", ...)` and appends log lines.
///
/// The projection to [`events::LogLine`] keeps the frontend from reaching into
/// the internally-tagged `WorkerEvent` enum: it reads a flat
/// `{ level, message }` and never invents text. The bridge never blocks the
/// event loop — `spawn_blocking` runs the blocking `recv` off the async
/// scheduler, and `emit` is a non-blocking fan-out.
#[cfg(all(unix, not(target_os = "macos")))]
fn install_event_bridge(
    app: tauri::AppHandle,
    events: std::sync::mpsc::Receiver<worker::WorkerEvent>,
) {
    tauri::async_runtime::spawn_blocking(move || {
        // `recv` blocks until an event arrives or the worker drops the sender
        // (at shutdown); both cases end the loop cleanly.
        while let Ok(event) = events.recv() {
            let line = event.to_log_line();
            if let Err(e) = tauri::Emitter::emit(&app, "worker-event", &line) {
                log::warn!("failed to emit worker event: {e}");
            }
        }
    });
}

/// Fallback path for targets without a native child-view presenter yet.
///
/// Builds the window + a fallback presenter that reports `Unsupported` (D-05)
/// so a Wayland-only / unported build still links and reports cleanly.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn run_skeleton(app: &mut tauri::App) -> Result<(), SkeletonError> {
    let (_window, rect) = build_window_and_chrome(app)?;
    let presenter = presenter::fallback::FallbackPresenter::new(
        "native child-view compositing is not yet implemented for this target (D-05)",
        rect,
    );
    // No device is created on this path: capability negotiation has no target
    // to negotiate against, so the recorded posture is emitted directly and the
    // process exits cleanly (never panics). Plan 05's gate report folds this
    // into the per-OS verdict.
    log::warn!(
        "D-05 fallback posture on this target: {}",
        presenter.reason()
    );
    Ok(())
}

/// Create the one resizable window plus the bottom-anchored chrome webview.
///
/// The webview occupies the bottom 208px (log 160 + control row 48) and leaves
/// the top region uncovered (UI-SPEC Surface Layout Contract; RESEARCH
/// Pitfall 6). Returns the window and the uncovered panorama rectangle.
fn build_window_and_chrome(
    app: &mut tauri::App,
) -> Result<(tauri::window::Window, ViewportRect), SkeletonError> {
    const WIDTH: f64 = 1280.0;
    const HEIGHT: f64 = 800.0;
    const CHROME_HEIGHT: f64 = (presenter::LOG_PANE_HEIGHT + presenter::CONTROL_ROW_HEIGHT) as f64;

    let window = tauri::window::Window::builder(app, "main")
        .title("Reco")
        .inner_size(WIDTH, HEIGHT)
        .min_inner_size(960.0, 600.0)
        .build()
        .map_err(|e| SkeletonError::Window(e.to_string()))?;

    window
        .add_child(
            tauri::webview::WebviewBuilder::new(
                "chrome",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .auto_resize(),
            tauri::LogicalPosition::new(0.0, HEIGHT - CHROME_HEIGHT),
            tauri::LogicalSize::new(WIDTH, CHROME_HEIGHT),
        )
        .map_err(|e| SkeletonError::Window(e.to_string()))?;

    Ok((
        window,
        ViewportRect::for_window(WIDTH as u32, HEIGHT as u32),
    ))
}

/// Typed errors at the binary edge.
#[derive(Debug, thiserror::Error)]
enum SkeletonError {
    /// Window/webview creation failed.
    #[error("window/webview creation failed: {0}")]
    Window(String),
    /// The presenter reported a typed error.
    #[error(transparent)]
    Presenter(#[from] PresenterError),
    /// GPU context creation failed.
    #[error(transparent)]
    Gpu(#[from] reco_core::gpu::GpuError),
    /// Calibration profile load failed.
    #[error(transparent)]
    Calibration(#[from] reco_core::calibration::CalibrationLoadError),
    /// Clip decode failed.
    #[error(transparent)]
    Source(#[from] reco_core::source::SourceError),
    /// The stitch pipeline failed.
    #[error(transparent)]
    Pipeline(#[from] reco_core::render::pipeline::PipelineError),
    /// Hardcoded path resolution failed.
    #[error(transparent)]
    Hardcoded(#[from] hardcoded::HardcodedError),
    /// Posting a command to the engine worker failed.
    #[error(transparent)]
    Worker(#[from] events::WorkerError),
}
