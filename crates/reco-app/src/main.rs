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

use presenter::{PresenterError, ViewportRect};
#[cfg(all(unix, not(target_os = "macos")))]
use presenter::{SurfacePresenter, x11::X11Presenter};
// `info()` / `next_frame()` are trait methods on `FfmpegFileSource`, not inherent
// ones — the trait must be in scope to call them (crates/reco-core/src/source.rs:345,351).
#[cfg(all(unix, not(target_os = "macos")))]
use reco_core::source::FrameSource as _;

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

/// Create the window, presenter, shared device, and drive one stitched frame.
///
/// Returns a typed error so the exact A1/A3 failure is visible (the caller logs
/// it; the gate report (Plan 05) folds it in).
#[cfg(all(unix, not(target_os = "macos")))]
fn run_skeleton(app: &mut tauri::App) -> Result<(), SkeletonError> {
    let (window, rect) = build_window_and_chrome(app)?;

    // Build the presenter first: the presenter owns its Surface (D-03), and
    // that surface drives adapter selection so the shared device is compatible
    // with it.
    let instance = reco_core::wgpu::Instance::default();
    let mut presenter = X11Presenter::new(&window, &instance, rect)?;

    // Create the device + adapter ONCE through the adapter-retaining
    // `for_surface` path (the presenter needs the adapter for
    // `get_capabilities`; RESEARCH Pitfall 4 / Open Question 3), then hand that
    // device to the presenter — never a second device (D-03/FOUND-03).
    let (gpu, surface_info) = pollster::block_on(reco_core::gpu::GpuContext::for_surface(
        &instance,
        presenter.surface(),
    ))?;

    let adapter = gpu.adapter().ok_or(SkeletonError::NoAdapterRetained)?;
    presenter.configure(gpu.device(), adapter, rect.width, rect.height)?;
    log::info!("surface format: {:?}", surface_info.format);

    drive_one_frame(gpu, surface_info.format, &mut presenter, rect)?;

    // Keep the window alive for the webview; the presenter is dropped at end of
    // setup (Phase 1 drives exactly one frame).
    drop(window);
    Ok(())
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

/// Decode one frame pair from the hardcoded clips and drive one stitched frame.
#[cfg(all(unix, not(target_os = "macos")))]
fn drive_one_frame(
    gpu: reco_core::gpu::GpuContext,
    surface_format: reco_core::wgpu::TextureFormat,
    presenter: &mut impl SurfacePresenter,
    rect: ViewportRect,
) -> Result<(), SkeletonError> {
    let paths = hardcoded::media_paths()?;
    let cal = reco_core::calibration::MatchCalibration::from_file(paths.calibration.as_path())?;

    let mut source = reco_io::adapters::FfmpegFileSource::open_with_offset(
        paths.left.as_path(),
        paths.right.as_path(),
        cal.sync_offset,
    )?;
    let info = source.info();

    let viewport = presenter::viewport_config(rect, 0.05, cal.rig_tilt as f32);
    let renderer = reco_core::render::stitch_renderer::StitchRenderer::new(
        cal,
        gpu,
        viewport,
        info.width,
        info.height,
        surface_format,
        reco_core::render::renderer::InputFormat::Yuv420p,
    )?;

    let frame = source.next_frame()?.ok_or(SkeletonError::NoFrames)?;
    let (left, right) = match frame {
        reco_core::source::StereoFrame::Yuv420p(pair) => (pair.left, pair.right),
        _ => return Err(SkeletonError::NotYuv420p),
    };

    presenter.render_frame(&renderer, &left, &right, 0.0, 0.0)?;
    log::info!("A1 verdict: native child view presented one stitched frame on Linux/X11");
    Ok(())
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
    /// The device-creation path did not retain an adapter (FOUND-04 gap).
    #[error("engine did not retain a wgpu::Adapter (adapter gap — see FRICTION.md)")]
    NoAdapterRetained,
    /// The hardcoded clips produced no frames.
    #[error("hardcoded clips produced no frames")]
    NoFrames,
    /// The clips did not decode to YUV420P.
    #[error("hardcoded clips did not decode to YUV420P")]
    NotYuv420p,
}
