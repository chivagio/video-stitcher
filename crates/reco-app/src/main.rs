//! `reco-app` — Tauri 2 desktop host that links the Reco engine crates
//! in-process and composites a native `wgpu::Surface` **below** the Tauri
//! webview.
//!
//! # Phase 2 preview shell (PREV-01, D-03)
//!
//! A full-window transparent Tauri webview is composited **above** a
//! Rust-owned native `wgpu::Surface` child view. The webview paints only the
//! opaque chrome (bottom transport bar + right controls rail + log drawer),
//! leaving the top-left preview region transparent so the native panorama
//! shows through; pointer events over that region reach the webview because the
//! native child is stacked below it.
//!
//! ```text
//!   Tauri/tao window (transparent)
//!   ├── chrome webview (full window, transparent; opaque chrome panels paint)
//!   └── native X11 child view (top-left region) ── owns wgpu::Surface
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
mod transport;
mod worker;

#[cfg(all(unix, not(target_os = "macos")))]
use presenter::x11::X11Presenter;
use presenter::{ChromeState, PresenterError, ViewportRect};
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
            commands::export,
            commands::play,
            commands::pause,
            commands::seek,
            commands::step_frame,
            commands::set_loop,
            commands::set_chrome,
            commands::set_presenter,
            commands::preview_attach_readback,
            commands::show_preview_window,
            commands::set_view,
            commands::intent,
            commands::republish_projection
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
    // Ordering is load-bearing (RESEARCH Pitfall 1): the native child view must
    // be created BEFORE the chrome webview so the webview sits ABOVE it in
    // z-order. An above-webview native child would swallow pointer events over
    // the preview region (breaking drag/wheel pose input, PREV-04).
    let window = build_window(app)?;

    // The native region is the L-shaped complement of the chrome (bottom
    // transport bar + right controls rail + log drawer), computed by the single
    // Rust geometry source of truth (UI-SPEC Geometry authority).
    let rect = ViewportRect::for_chrome(1280, 800, &ChromeState::default());

    // Build the presenter before the webview: the presenter owns its Surface
    // (D-03), and that surface drives adapter selection when the worker creates
    // the device. It is explicitly lowered below the webview before the webview
    // is added, so pointer events reach the chrome.
    let instance = reco_core::wgpu::Instance::default();
    // The native arm is the strongest (zero-copy) presenter but is available
    // only on an X11/Xlib parent handle. Attempt it first and, on a capability
    // fall-through (e.g. a native Wayland handle, D-05), record the reason and
    // omit the arm — mirroring 02-06's catch-and-omit shape for the
    // separate-window arm. A non-fall-through construction failure stays fatal.
    let mut native_error: Option<PresenterError> = None;
    let native_presenter: Option<X11Presenter> = match X11Presenter::new(&window, &instance, rect) {
        Ok(mut presenter) => {
            presenter.lower();
            Some(presenter)
        }
        Err(e) if presenter::is_fallthrough(&e) => {
            log::warn!("native presenter unavailable, falling back: {e}");
            native_error = Some(e);
            None
        }
        Err(e) => return Err(SkeletonError::Presenter(e)),
    };

    // Pre-create the Rust-owned, webview-less preview window on the SETUP thread
    // (PREV-05 / RESEARCH Pattern 5): it starts hidden, and the separate-window
    // presenter is built from it here. Creating windows and wgpu surfaces off the
    // setup thread is forbidden (macOS surface panic; Tauri's Windows
    // window-creation deadlock), so the worker only ever *installs* a
    // pre-built presenter, never constructs one. The presenter chain (Task 3)
    // selects among these; here they are prepared and handed to the app.
    let preview_window = build_preview_window(app)?;
    let separate_window_presenter = match presenter::separate_window::SeparateWindowPresenter::new(
        preview_window,
        &instance,
        rect,
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("separate-window presenter unavailable, falling back to readback: {e}");
            None
        }
    };

    // Full-window transparent chrome webview: it is an absolutely positioned set
    // of opaque panels that tiles around the transparent preview hole. Pointer
    // events over the preview region land on this webview (it is above).
    add_chrome_webview(app, &window)?;

    // The presenter chain (PREV-05): native → separate window → readback. All
    // three are pre-created here on the setup thread; the worker installs the
    // active one and can swap at a tick boundary on a manual override (Task 3).
    // Readback is trivially cheap (no surface; it shares the worker's device).
    //
    // The chain is built conditionally from the arms that constructed, in fixed
    // strongest-first order — Native → SeparateWindow → Readback — because the
    // worker activates the FIRST entry it receives.
    let mut presenter_chain: worker::PresenterChain = Vec::new();
    if let Some(native) = native_presenter {
        presenter_chain.push((presenter::PresenterKind::Native, Box::new(native)));
    }
    if let Some(separate) = separate_window_presenter {
        presenter_chain.push((presenter::PresenterKind::SeparateWindow, Box::new(separate)));
    }
    presenter_chain.push((
        presenter::PresenterKind::Readback,
        Box::new(presenter::readback::ReadbackPresenter::new(rect)),
    ));

    // The startup fall-through reason: non-`None` only when the native arm did
    // not construct, so the worker's activated presenter (the chain head) is a
    // fallback from native compositing. Carried to the worker, which re-asserts
    // it (with the resolved kind) through `republish_projection` after the
    // webview subscribes — the single locked-WARN delivery point.
    let chain_kinds: Vec<presenter::PresenterKind> =
        presenter_chain.iter().map(|(kind, _)| *kind).collect();
    let startup_fallback = presenter::startup_fallback_reason(&chain_kinds, native_error.as_ref());

    // Ownership handoff (FOUND-03): the device is created *inside* the worker
    // from the presenter's surface, so the worker is the sole device owner.
    // Nothing on this (the setup) thread holds a device handle or renders
    // directly. The readback sender is the worker's channel path for
    // `preview_attach_readback` (PREV-05).
    let (worker, events, readback_tx) =
        worker::spawn_gpu_worker(instance, presenter_chain, rect, startup_fallback)?;

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
    //
    // The same handler also forwards window resizes to the worker as
    // `ResizeViewport`, so the native viewport is reconfigured at a command
    // boundary (Plan 02-02 Task 3) — the frontend/webview never sizes the
    // native view.
    install_close_handler(&window, worker, handle.clone());

    // Auto-import the hardcoded clips on startup (D-08). Phase 2's UI has no
    // Import button — the transport bar is Play/Pause/Step/Loop — so the app
    // must import automatically for Play to have a session to start. This is
    // consistent with the hardcoded-clips design: the app is a preview tool,
    // not a file picker (that is Phase 3). The import is fire-and-forget: the
    // worker loads the clips and calibration, and the frontend receives the
    // transport/position events that transition the UI from "empty" to "ready".
    //
    // A closed channel here means the worker already exited during setup; that
    // is not recoverable from this thread, so log it and let the frontend's
    // log drawer show the failure rather than panicking in `setup`.
    if let Err(e) = handle.send(commands::WorkerCommand::Import) {
        log::warn!("failed to post the startup import to the worker: {e}");
    }

    // The thin path is driven by the WEBVIEW, not from here: the
    // `#[tauri::command]` handlers post Preview / Export / Play / Pause / Seek /
    // Step / Loop / SetView / Intent when the user presses the matching button
    // (D-06/D-09). We deliberately do NOT auto-post Preview at startup — the
    // user drives the engine without a CLI, and the busy/disabled interaction
    // contract proves the single-owner boundary as each command is issued.

    // Keep the window (whose child the worker renders into) alive for the app's
    // lifetime; the webview owns the lifetime from here.
    app.manage(handle);
    app.manage(worker::ReadbackSender(readback_tx));
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

/// Register the window-close handler that performs clean teardown (FOUND-06),
/// and forward window resizes to the worker (Plan 02-02 Task 3).
///
/// On [`tauri::WindowEvent::CloseRequested`] / `Destroyed`, the worker is asked
/// to stop and joined within [`SHUTDOWN_TIMEOUT`]. The drop order is enforced by
/// the worker's field order and `shutdown()` (stop loop → drop decode → drop
/// renderer → drop surface → drop device) — see `worker.rs` for the NVDEC/wgpu
/// hazard citation. The event bridge task ends when the worker drops its event
/// sender, so the process can exit.
///
/// On [`tauri::WindowEvent::Resized`], a [`WorkerCommand::ResizeViewport`] is
/// posted through `resize_handle`; the worker recomputes the native viewport
/// from the new size + reported chrome state and reconfigures the surface,
/// without resetting the playhead or pose.
#[cfg(all(unix, not(target_os = "macos")))]
fn install_close_handler(
    window: &tauri::window::Window,
    worker: worker::EngineWorker,
    resize_handle: worker::WorkerHandle,
) {
    let state = TeardownState {
        worker: std::sync::Mutex::new(Some(worker)),
    };
    // The state is moved into the handler and returned to Tauri via `manage` so
    // it is not dropped before the event fires.
    let shared = std::sync::Arc::new(state);
    window.app_handle().manage(shared.clone());

    window.on_window_event(move |event| {
        match event {
            tauri::WindowEvent::Resized(size) => {
                // Forward the new physical size to the worker. A zero size is
                // ignored (a minimized window reports 0x0; `for_chrome`
                // saturates, but reconfiguring to zero is pointless).
                if size.width > 0
                    && size.height > 0
                    && let Err(e) = resize_handle.send(worker::WorkerCommand::ResizeViewport {
                        width: size.width,
                        height: size.height,
                    })
                {
                    log::warn!("failed to forward window resize to the worker: {e}");
                }
            }
            tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed => {
                // Take the worker once; a second close event is a no-op.
                let taken = shared.worker.lock().ok().and_then(|mut guard| guard.take());
                if let Some(worker) = taken {
                    match worker.shutdown(SHUTDOWN_TIMEOUT) {
                        Ok(()) => log::info!("engine worker stopped cleanly"),
                        Err(e) => log::warn!("engine worker shutdown: {e}"),
                    }
                }
            }
            _ => {}
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
    let _window = build_window(app)?;
    let rect = ViewportRect::for_chrome(1280, 800, &ChromeState::default());
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

/// Create the one resizable, transparent window (no chrome yet).
///
/// The window is created first and the native presenter's child view is added
/// on top of it (in `run_skeleton`) *before* the chrome webview, so the webview
/// is the topmost layer and receives pointer events over the preview region
/// (RESEARCH Pitfall 1). `transparent(true)` is what allows the webview to
/// leave the native preview region unpainted (Phase 2 Surface Layout Contract).
fn build_window(app: &mut tauri::App) -> Result<tauri::window::Window, SkeletonError> {
    const WIDTH: f64 = 1280.0;
    const HEIGHT: f64 = 800.0;

    tauri::window::Window::builder(app, "main")
        .title("Reco")
        .inner_size(WIDTH, HEIGHT)
        .min_inner_size(960.0, 600.0)
        .transparent(true)
        .build()
        .map_err(|e| SkeletonError::Window(e.to_string()))
}

/// Add the full-window transparent chrome webview on top of the native view.
///
/// The webview covers the whole window and is asked to be transparent; the
/// chrome markup paints only the opaque bottom transport strip and right
/// controls rail, leaving the top-left preview region (the native panorama)
/// unpainted. Pointer events over the preview region land on this webview
/// because the native child was lowered below it (RESEARCH Pitfall 1/2).
///
/// The webview is explicitly built full-window and `.auto_resize()`d so it
/// tracks the window on resize; the native child viewport is recomputed and the
/// surface reconfigured by the worker's resize path (Plan 02-02+).
fn add_chrome_webview(
    _app: &mut tauri::App,
    window: &tauri::window::Window,
) -> Result<(), SkeletonError> {
    const WIDTH: f64 = 1280.0;
    const HEIGHT: f64 = 800.0;

    window
        .add_child(
            tauri::webview::WebviewBuilder::new(
                "chrome",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .transparent(true)
            .auto_resize(),
            tauri::LogicalPosition::new(0.0, 0.0),
            tauri::LogicalSize::new(WIDTH, HEIGHT),
        )
        .map_err(|e| SkeletonError::Window(e.to_string()))?;

    Ok(())
}

/// Create the hidden, webview-less preview window used by the separate-window
/// presenter (PREV-05).
///
/// Built on the setup thread because Tauri forbids synchronous window creation
/// off it (Windows deadlock) and wgpu forbids non-main-thread surface creation on
/// macOS. It starts hidden (`visible(false)`) and is shown by the separate-window
/// presenter's `show` action.
fn build_preview_window(app: &tauri::App) -> Result<tauri::window::Window, SkeletonError> {
    tauri::window::WindowBuilder::new(app, presenter::separate_window::PREVIEW_WINDOW_LABEL)
        .title("Reco Preview")
        .inner_size(1280.0, 720.0)
        .visible(false)
        .build()
        .map_err(|e| SkeletonError::Window(e.to_string()))
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
