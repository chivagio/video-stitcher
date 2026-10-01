//! Rust-owned surface presenter (D-02).
//!
//! # The two-layer model
//!
//! Phase 1 composites two independent layers inside **one** OS window
//! (CONTEXT D-01/D-02/D-03):
//!
//! ```text
//!   one Tauri/tao window
//!   ├── chrome webview (full window, transparent; opaque panels paint)
//!   │     bottom transport bar 72px · right controls rail 40/280px · log drawer 240/0px
//!   └── native child view (top-left region) ── owns a wgpu::Surface
//!                                               render target = stitched panorama
//! ```
//!
//! The webview leaves the top-left preview region uncovered (UI-SPEC Surface
//! Layout Contract); the presenter renders the panorama into a **native child
//! view** positioned in that region. The webview and the panorama are
//! independent layers — the frontend never sizes, moves, or creates the native
//! view, and no raw handle (`raw-window-handle` / HWND / NSView / X11) is ever
//! exposed across IPC (D-02).
//!
//! # Device sharing, not device ownership
//!
//! D-03: the presenter owns its own [`wgpu::Surface`] but **shares the engine
//! worker's device and queue**. The engine worker remains the single device
//! owner (FOUND-03); the presenter is a render target the worker draws into.
//! The presenter therefore never creates its own GPU device — it borrows the
//! device and queue from the worker's GPU context and negotiates surface
//! capabilities against the worker's retained adapter.
//!
//! The trait surface ([`SurfacePresenter`]) is the seam Phase 2's
//! runtime-swappable presenter (PREV-05: native compositing → separate preview
//! window → throttled readback) extends.

use reco_core::render::stitch_renderer::StitchRenderer;
use reco_core::render::viewport::ViewportConfig;
use reco_core::source::YuvData;

#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;

pub mod fallback;
pub mod readback;
pub mod separate_window;

/// Compile-time check that the presenter can be moved to the engine worker
/// thread.
///
/// The worker takes the presenter as `Box<dyn SurfacePresenter + Send>` — the
/// `Send` bound is mandatory because the presenter is moved onto the worker
/// thread (D-03), which is the only thread that draws into its surface. This
/// assertion fails the build if a platform impl ever stops being `Send`.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    #[cfg(all(unix, not(target_os = "macos")))]
    assert_send::<x11::X11Presenter>();
    assert_send::<fallback::FallbackPresenter>();
    assert_send::<separate_window::SeparateWindowPresenter>();
    assert_send::<readback::ReadbackPresenter>();
};

/// The platform-native presenter selected at compile time.
///
/// `#[cfg]`-gated by target — never a runtime `match target_os`
/// (CONVENTIONS.md / ARCHITECTURE.md). Windows/macOS arm in as those impls
/// land; targets without a native child-view impl fall back to
/// [`fallback::FallbackPresenter`] (D-05).
// `PlatformPresenter` names the compile-time-selected impl as a convenience for
// consumers; Phase 1's single `run_skeleton` path constructs the concrete type
// directly. Kept as the documented seam Phase 2 selects through (D-02).
#[allow(dead_code)]
#[cfg(all(unix, not(target_os = "macos")))]
pub type PlatformPresenter = x11::X11Presenter;

/// The platform-native presenter on targets with no native child-view impl.
#[allow(dead_code)]
#[cfg(not(all(unix, not(target_os = "macos"))))]
pub type PlatformPresenter = fallback::FallbackPresenter;

/// Height in logical pixels of the webview transport bar (UI-SPEC).
///
/// Bottom-anchored, full width: timeline row (24px) + control row (48px).
pub const TRANSPORT_BAR_HEIGHT: u32 = 72;

/// Width in logical pixels of the expanded right controls panel (UI-SPEC).
pub const CONTROLS_PANEL_WIDTH: u32 = 280;

/// Width in logical pixels of the collapsed right controls rail (UI-SPEC).
pub const CONTROLS_PANEL_COLLAPSED_WIDTH: u32 = 40;

/// Height in logical pixels of the expanded event-log drawer (UI-SPEC).
///
/// Zero when collapsed (the default).
pub const LOG_DRAWER_HEIGHT: u32 = 240;

/// Which presenter in the PREV-05 chain is active or requested.
///
/// The chain is fixed and ordered strong→weak:
/// `Native` (zero-copy child view) → `SeparateWindow` (zero-copy Rust window) →
/// `Readback` (throttled, the only mode where frame pixels cross IPC). Serde
/// snake_case so it round-trips through the typed `set_presenter` command and
/// the `Presenter` event without stringly-typed handling (T-02-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PresenterKind {
    /// Native child-view compositing (zero-copy; the PREV-01 default).
    Native,
    /// A separate, Rust-owned, webview-less preview window (zero-copy).
    SeparateWindow,
    /// Throttled CPU readback pushed to the webview (degraded fallback).
    Readback,
}

impl PresenterKind {
    /// The locked UI-SPEC badge label for this presenter.
    ///
    /// Consumed by the UI (through the serialized `kind`) and asserted by this
    /// module's tests to lock the copy.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn label(&self) -> &'static str {
        match self {
            PresenterKind::Native => "Presenter: Native",
            PresenterKind::SeparateWindow => "Presenter: Separate window",
            PresenterKind::Readback => "Presenter: Readback",
        }
    }

    /// The short human name used in log lines ("Separate window" / "Readback").
    pub fn name(&self) -> &'static str {
        match self {
            PresenterKind::Native => "Native",
            PresenterKind::SeparateWindow => "Separate window",
            PresenterKind::Readback => "Readback",
        }
    }
}

/// The fixed PREV-05 presenter chain order, strongest-first.
///
/// [`choose_presenter`] walks this order; the chain fall-through set is
/// [`PresenterError::Unsupported`] `|` [`PresenterError::ChildView`] `|`
/// [`PresenterError::Surface`] `|` [`PresenterError::Window`].
pub const PRESENTER_CHAIN: [PresenterKind; 3] = [
    PresenterKind::Native,
    PresenterKind::SeparateWindow,
    PresenterKind::Readback,
];

/// Whether a presenter-construction error forces the chain to fall through to
/// the next step.
///
/// The fall-through set (per the plan's PREV-05 assumption):
/// `Unsupported` (no child embedding), `ChildView` (child creation failed),
/// `Surface` (surface build failed), `Window` (preview-window build failed).
/// The remaining variants are runtime state (`NotConfigured`, `SurfaceLost`)
/// and never occur during construction, so they do not participate.
pub fn is_fallthrough(error: &PresenterError) -> bool {
    matches!(
        error,
        PresenterError::Unsupported { .. }
            | PresenterError::ChildView { .. }
            | PresenterError::Surface { .. }
            | PresenterError::Window { .. }
    )
}

/// Decide which presenter the chain should activate from a sequence of attempts.
///
/// `attempts` is the chain walked in order; each entry records whether that step
/// succeeded. The decision is **pure** (no GPU, no window): it returns the first
/// step that succeeded, or — if every step failed — the last step's kind (the
/// weakest) so the caller always has a definite presenter to report. This is the
/// unit-testable core of the probe-by-attempt protocol (RESEARCH Pattern 2):
/// the platform is never pre-selected by `WAYLAND_DISPLAY`/`XDG_SESSION_TYPE`.
pub fn choose_presenter(
    attempts: &[(PresenterKind, Result<(), PresenterError>)],
) -> (PresenterKind, Option<PresenterError>) {
    // First success wins.
    for (kind, result) in attempts {
        if result.is_ok() {
            return (*kind, None);
        }
    }
    // Every step failed: report the last (weakest) kind with its reason.
    match attempts.last() {
        Some((kind, Err(e))) => (*kind, Some(e.clone())),
        // An empty attempt list is a programmer error; default to the weakest
        // presenter rather than panicking.
        _ => (PresenterKind::Readback, None),
    }
}

/// The UI-SPEC-locked remediation clause for a fallback to `kind`.
///
/// Exact copy shape (Presenter & Degradation Contract):
/// `Presenter fallback to <Separate window|Readback>: <reason>. <remediation>.`
pub fn fallback_remediation(kind: PresenterKind) -> &'static str {
    match kind {
        PresenterKind::Native => "Native compositing is active.",
        PresenterKind::SeparateWindow => "The panorama is shown in a separate preview window.",
        PresenterKind::Readback => {
            "Preview is throttled. Use a separate preview window for smoother playback."
        }
    }
}

/// Build the locked WARN line for a fallback to `kind` with `reason`.
///
/// A single, exact-shape line so degradation is never silent and the copy is
/// identical wherever a fallback is reported (UI-SPEC; Phase 1 D-05).
pub fn fallback_warn_line(kind: PresenterKind, reason: &str) -> String {
    format!(
        "Presenter fallback to {}: {}. {}",
        kind.name(),
        reason.trim_end_matches('.'),
        fallback_remediation(kind)
    )
}

/// The webview chrome's collapsible state (UI-SPEC Surface Layout Contract).
///
/// This is the Rust-side source of truth for the *native* viewport geometry;
/// the frontend reports its chrome state (panel/drawer open/closed) and Rust
/// recomputes [`ViewportRect::for_chrome`]. The frontend must never compute the
/// native rect itself (UI-SPEC "Geometry authority").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChromeState {
    /// Whether the right controls panel is expanded (vs. the 40px rail).
    ///
    /// Defaults to collapsed (`false`), matching the UI-SPEC default panel
    /// width of 40px — the panel is opt-in so the panorama gets the space by
    /// default.
    pub panel_expanded: bool,
    /// Whether the event-log drawer is expanded.
    ///
    /// Defaults to collapsed (`false`), which the UI-SPEC fixes as the drawer's
    /// default (drawer height 0). Opening it shrinks the native viewport.
    pub drawer_expanded: bool,
}

impl ChromeState {
    /// The right controls panel's width in physical pixels for this state.
    pub fn panel_width(&self) -> u32 {
        if self.panel_expanded {
            CONTROLS_PANEL_WIDTH
        } else {
            CONTROLS_PANEL_COLLAPSED_WIDTH
        }
    }

    /// The event-log drawer's height in physical pixels for this state.
    pub fn drawer_height(&self) -> u32 {
        if self.drawer_expanded {
            LOG_DRAWER_HEIGHT
        } else {
            0
        }
    }
}

/// Ground colour of the idle/clear frame painted before the first stitched
/// frame (UI-SPEC E3; the `#1e1e1e` app ground).
///
/// Kept as a linear-ish 0..1 RGBA tuple so the presenter can clear its surface
/// without a shader pass.
pub const IDLE_CLEAR_COLOR: [f64; 4] = [30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 1.0];

/// Classify a `wgpu::SurfaceError` into the FOUND-05 recovery vocabulary.
///
/// `wgpu::SurfaceError` is not `Clone`, so the presenter classifies it at its
/// boundary and the worker branches on the resulting [`SurfaceErrorKind`].
/// This is the single mapping point shared by every platform presenter impl.
pub fn classify_surface_error(error: &reco_core::wgpu::SurfaceError) -> SurfaceErrorKind {
    use reco_core::wgpu::SurfaceError;
    match error {
        SurfaceError::Outdated => SurfaceErrorKind::Outdated,
        SurfaceError::Lost => SurfaceErrorKind::Lost,
        SurfaceError::Timeout => SurfaceErrorKind::Timeout,
        SurfaceError::OutOfMemory => SurfaceErrorKind::OutOfMemory,
        _ => SurfaceErrorKind::Other,
    }
}

/// Typed presenter error.
///
/// `Clone + Send + Sync` so it can cross the worker channel and be rendered by
/// the webview (CONVENTIONS.md) — it is a value, not a live I/O error, and it
/// never carries a raw OS handle.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PresenterError {
    /// The native child view could not be created from the parent handle.
    #[error("child view creation failed: {reason}")]
    ChildView {
        /// Human-readable reason, safe to log and display.
        reason: String,
    },

    /// The target cannot host a native child view at all (e.g. Wayland).
    ///
    /// This is a **recorded posture**, not a crash: it is what the fallback
    /// presenter returns so a Wayland-only build links and reports cleanly
    /// (D-05).
    #[error("presenter unsupported on this target: {reason}")]
    Unsupported {
        /// Why the target is unsupported.
        reason: String,
    },

    /// The Rust-owned Tauri preview window (separate-window presenter) could not
    /// be created or converted into a `wgpu::Surface`.
    ///
    /// Distinct from [`PresenterError::ChildView`] (an X11 child view) and
    /// [`PresenterError::Surface`] (a `wgpu::Surface` failure): this is the
    /// window-creation failure path for PREV-05's separate-window presenter, so
    /// the chain driver can fall through to readback with a typed reason.
    #[error("preview window creation failed: {reason}")]
    Window {
        /// Human-readable reason, safe to log and display.
        reason: String,
    },

    /// Building the `wgpu::Surface` from the child handle failed.
    #[error("surface creation failed: {reason}")]
    Surface {
        /// Human-readable reason.
        reason: String,
    },

    /// The presenter had no surface configured yet.
    #[error("presenter is not configured")]
    NotConfigured,

    /// The surface is no longer usable and requires a recovery action
    /// (reconfigure for `Outdated`, device rebuild for `Lost`).
    ///
    /// FOUND-05: the frame path branches on [`SurfaceErrorKind`] rather than
    /// panicking or presenting a black frame. See
    /// `crates/reco-cli/src/preview.rs:806-883` for the in-repo precedent.
    #[error("surface error ({kind:?}) — recovery required")]
    SurfaceLost {
        /// Which `wgpu::SurfaceError` variant triggered the recovery.
        kind: SurfaceErrorKind,
    },
}

/// The recovery-relevant classification of a `wgpu::SurfaceError`.
///
/// FOUND-05 requires the frame path to branch **per variant**, so the raw
/// `wgpu::SurfaceError` (which is not `Clone`) is classified at the presenter
/// boundary into a `Clone + Send + Sync` value the worker can act on:
///
/// * [`SurfaceErrorKind::Outdated`] → reconfigure the existing device's surface.
/// * [`SurfaceErrorKind::Lost`] → rebuild the device, then reconfigure.
/// * [`SurfaceErrorKind::Timeout`] → skip the frame (transient; retry next tick).
/// * [`SurfaceErrorKind::OutOfMemory`] → log and skip the frame.
/// * [`SurfaceErrorKind::Other`] → log and skip the frame.
///
/// Mirrors the CLI preview's handling (`crates/reco-cli/src/preview.rs:806-883`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SurfaceErrorKind {
    /// The swapchain is out of date (e.g. the window resized): reconfigure.
    Outdated,
    /// The surface was lost (device lost / window invalidated): rebuild.
    Lost,
    /// Acquiring the next frame timed out: skip this frame.
    Timeout,
    /// The surface ran out of memory: log and skip this frame.
    OutOfMemory,
    /// Any other surface error: log and skip this frame.
    Other,
}

/// How the frame path should treat a non-presented frame.
///
/// A [`FrameOutcome::Skipped`] is the FOUND-05 contract for the transient
/// surface errors: the frame is dropped (never presented, never black) and the
/// caller simply continues the loop. Recovery actions (`Outdated`/`Lost`) are
/// *not* skips — they surface as [`PresenterError::SurfaceLost`] so the worker
/// performs the reconfigure/rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The frame was rendered and presented.
    Presented,
    /// The frame was dropped with no recovery action (retry next tick).
    Skipped {
        /// The classified surface error that caused the skip.
        kind: SurfaceErrorKind,
    },
}

/// Geometry of the native panorama viewport within the window.
///
/// The viewport fills the window **minus** the webview chrome, which is
/// bottom-anchored (UI-SPEC Surface Layout Contract): the panorama is anchored
/// top-left and expands right and down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportRect {
    /// Offset from the window's left edge (always 0 in Phase 1).
    pub x: u32,
    /// Offset from the window's top edge (always 0 in Phase 1).
    pub y: u32,
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

impl ViewportRect {
    /// Compute the panorama viewport for a window of `width` × `height`
    /// physical pixels given the webview chrome's collapsible [`ChromeState`].
    ///
    /// The rect is **top-left anchored** and is the L-shaped complement of the
    /// chrome (UI-SPEC Surface Layout Contract):
    ///
    /// ```text
    ///   width  = window_w − chrome.panel_width()          (right rail/panel)
    ///   height = window_h − TRANSPORT_BAR_HEIGHT − drawer_height()  (bottom bar + drawer)
    /// ```
    ///
    /// Saturates at every step rather than underflowing when the window is
    /// smaller than the chrome (a minimum window size is enforced by
    /// `tauri.conf.json`, but the computation must still be total).
    pub fn for_chrome(window_w: u32, window_h: u32, chrome: &ChromeState) -> Self {
        Self {
            x: 0,
            y: 0,
            width: window_w.saturating_sub(chrome.panel_width()),
            height: window_h
                .saturating_sub(TRANSPORT_BAR_HEIGHT)
                .saturating_sub(chrome.drawer_height()),
        }
    }

    /// Whether the viewport has a drawable (non-zero) area.
    ///
    /// A window can legitimately shrink below the chrome reservation, in which
    /// case the panorama region is empty. Exercised by this module's tests; the
    /// binary path always passes a 1280x800 viewport.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_drawable(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// A render target owned by Rust for the stitched panorama (D-02).
///
/// Implementors hide all platform-specific child-view and raw-handle plumbing;
/// callers (the engine worker) only see configure / render / resize.
///
/// # Threading
///
/// A presenter is constructed on the setup thread (the window handle is only
/// available there) and then **moved onto the engine worker thread**, which is
/// the only thread that draws into it. Platform impls must therefore be
/// `Send`; the worker boxes them as `Box<dyn SurfacePresenter + Send>` and
/// [`x11::X11Presenter`] carries a compile-time `Send` assertion.
pub trait SurfacePresenter {
    /// The presenter's surface, for device creation, or `None` when this
    /// presenter is **headless** (PREV-05's readback presenter paints no
    /// surface).
    ///
    /// The worker calls this **once** to create the shared device: `Some(surface)`
    /// routes through the engine's surface-compatible GPU-context constructor
    /// (`GpuContext::for_surface`); `None` routes through the headless
    /// constructor (`GpuContext::with_surface(None)`), which still retains the
    /// adapter so `configure`/`rebind_instance` keep working.
    ///
    /// The surface never leaves the Rust process and is never exposed across IPC
    /// (D-02/D-03). Implementors keep ownership.
    ///
    /// A presenter that *can* host a surface returns `Some`; only the readback
    /// presenter returns `None`.
    fn surface(&self) -> Option<&reco_core::wgpu::Surface<'static>>;

    /// Rebuild this presenter's surface on a *fresh* `wgpu::Instance`.
    ///
    /// Called during `Lost` recovery (FOUND-05) with the **resolved
    /// `rebuild-reconfigure` contract**: the window-derived child view survives
    /// a device loss, but both the `Instance` and the `Surface` created from it
    /// are bound to the lost parent device and cannot be reused — a fresh
    /// `Instance` cannot adopt a surface created by another instance, and
    /// requesting an adapter from the stale instance fails with
    /// "Parent device is lost". So the presenter recreates the surface from the
    /// still-valid child window onto the new instance. The window is NOT
    /// recreated (no flicker, no second window — D-01/D-02 hold).
    ///
    /// Implementors keep ownership of the new surface.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::Surface`] if the surface cannot be rebuilt, or
    /// [`PresenterError::Unsupported`] on a target with no surface (D-05).
    fn rebind_instance(
        &mut self,
        instance: &reco_core::wgpu::Instance,
    ) -> Result<(), PresenterError>;

    /// Configure (or reconfigure) the surface against the shared device.
    ///
    /// Must be called with the **worker's** device/queue and retained adapter —
    /// never a freshly-created second device or adapter (D-03).
    ///
    /// Called again after [`PresenterError::SurfaceLost`] recovery: for
    /// `Outdated` with the same device, for `Lost` with the rebuilt device.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::Surface`] if the surface cannot be configured,
    /// or [`PresenterError::NotConfigured`] if no platform surface exists.
    fn configure(
        &mut self,
        device: &reco_core::wgpu::Device,
        queue: &reco_core::wgpu::Queue,
        adapter: &reco_core::wgpu::Adapter,
        rect: ViewportRect,
    ) -> Result<(), PresenterError>;

    /// Render one stitched frame into this presenter's surface.
    ///
    /// `left` / `right` are the decoded YUV420P planes; `yaw` / `pitch` / `fov_degrees`
    /// position the panorama. Implementations acquire the next surface texture,
    /// render through the shared [`StitchRenderer`], and present it.
    ///
    /// `renderer` is taken by `&mut` so the readback path can call
    /// [`StitchRenderer::render_and_readback_rgba`] (which requires `&mut self`)
    /// and so FOV can be plumbed uniformly across every impl (PREV-04/PREV-05).
    ///
    /// The return value follows the FOUND-05 contract:
    ///
    /// * [`FrameOutcome::Presented`] — a frame reached the swapchain (or, for the
    ///   readback presenter, a frame was produced for display).
    /// * [`FrameOutcome::Skipped`] — a transient surface error (`Timeout` /
    ///   `OutOfMemory` / `Other`) dropped this frame; retry next tick.
    /// * `Err(`[`PresenterError::SurfaceLost`]`)` — the surface needs a
    ///   recovery action; the caller reconfigure/rebuilds **before** the next
    ///   `render_frame`. The frame is **never** held across that recovery.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::NotConfigured`] if the surface is not
    /// configured, [`PresenterError::SurfaceLost`] on an `Outdated`/`Lost`
    /// surface error, or [`PresenterError::Surface`] on a render failure.
    fn render_frame(
        &mut self,
        renderer: &mut StitchRenderer,
        left: &YuvData,
        right: &YuvData,
        yaw: f32,
        pitch: f32,
        fov_degrees: f32,
    ) -> Result<FrameOutcome, PresenterError>;

    /// Paint an idle/clear frame into the reserved region (UI-SPEC E3).
    ///
    /// Called **before the first stitched frame** so the panorama region is
    /// never an undefined/black hole at startup. The frame is a solid ground
    /// colour ([`IDLE_CLEAR_COLOR`]); it carries no data and is not a
    /// placeholder for content, it is the documented idle state.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::NotConfigured`] if the surface is not
    /// configured, or [`PresenterError::Surface`] on a render failure.
    fn render_idle(&mut self) -> Result<(), PresenterError>;

    /// Resize the presenter's surface to the geometry of `rect`.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::Surface`] if the surface cannot be
    /// reconfigured.
    // Part of the presenter contract (D-02) that Phase 2's runtime-swappable
    // presenter and window-resize handling drive. Phase 1's single-frame
    // `run_skeleton` path never resizes; every impl provides it, and the
    // fallback impl's version is covered by this module's tests.
    #[cfg_attr(not(test), allow(dead_code))]
    fn resize(&mut self, rect: ViewportRect) -> Result<(), PresenterError>;

    /// The current viewport geometry.
    // Part of the presenter contract (D-02); see [`Self::resize`].
    #[cfg_attr(not(test), allow(dead_code))]
    fn viewport(&self) -> ViewportRect;

    /// Release the platform child window now, while its parent is still alive
    /// (FOUND-06).
    ///
    /// Destroying the child window only from `Drop` risks running after GTK has
    /// torn the parent down, which raises a fatal `BadDrawable` X error and kills
    /// the process before a clean stop can be reported. Targets whose presenter
    /// owns no separate OS window use the default no-op.
    ///
    /// Idempotent: a second call must be a no-op.
    fn release_presenter_window(&mut self) {}

    /// Attach a webview readback channel (PREV-05).
    ///
    /// Only the readback presenter stores it; every other impl uses the default
    /// no-op, so the worker can hand a channel to whichever inactive presenter is
    /// the readback one without a downcast.
    fn attach_readback_channel(&mut self, _channel: tauri::ipc::Channel<tauri::ipc::Response>) {}

    /// The texture format this presenter renders its target in, once configured.
    ///
    /// `Some` for a surface-backed presenter (its negotiated format) and for the
    /// readback presenter (`Rgba8Unorm`, its internal target); `None` before
    /// `configure` or for a presenter with no render target. Used when swapping
    /// presenters so the shared renderer is rebuilt in the right format.
    fn configured_format(&self) -> Option<reco_core::wgpu::TextureFormat> {
        None
    }

    /// Show this presenter's own preview window (PREV-05).
    ///
    /// Only the separate-window presenter has one; every other impl uses the
    /// default no-op, so the worker can route the "Show preview window" action
    /// without a downcast.
    fn show_preview_window(&self) -> Result<(), PresenterError> {
        Ok(())
    }
}

/// Build a [`ViewportConfig`] matching the presenter's panorama viewport.
///
/// Kept here (not in a platform module) so every platform impl produces the
/// identical viewport for the shared [`StitchRenderer`].
pub fn viewport_config(rect: ViewportRect, blend_width: f32, rig_tilt: f32) -> ViewportConfig {
    ViewportConfig {
        width: rect.width,
        height: rect.height,
        blend_width,
        rig_tilt,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_srgb_selects_linear_format() {
        use reco_core::wgpu::TextureFormat;
        assert_eq!(
            StitchRenderer::strip_srgb(TextureFormat::Rgba8UnormSrgb),
            TextureFormat::Rgba8Unorm
        );
        assert_eq!(
            StitchRenderer::strip_srgb(TextureFormat::Bgra8UnormSrgb),
            TextureFormat::Bgra8Unorm
        );
    }

    #[test]
    fn strip_srgb_passes_through_unaffected_format() {
        use reco_core::wgpu::TextureFormat;
        assert_eq!(
            StitchRenderer::strip_srgb(TextureFormat::Rgba8Unorm),
            TextureFormat::Rgba8Unorm
        );
    }

    #[test]
    fn for_chrome_uses_saturating_l_shaped_geometry() {
        // Default chrome: collapsed 40px right rail + 72px bottom transport bar,
        // drawer collapsed (0). UI-SPEC Surface Layout Contract.
        let rect = ViewportRect::for_chrome(1280, 800, &ChromeState::default());
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, 0);
        assert_eq!(rect.width, 1280 - 40);
        assert_eq!(rect.height, 800 - 72);
    }

    #[test]
    fn for_chrome_expanded_panel_and_drawer_shrink_the_viewport() {
        let chrome = ChromeState {
            panel_expanded: true,
            drawer_expanded: true,
        };
        let rect = ViewportRect::for_chrome(1280, 800, &chrome);
        assert_eq!(rect.width, 1280 - 280);
        assert_eq!(rect.height, 800 - 72 - 240);
    }

    #[test]
    fn for_chrome_saturates_and_never_underflows() {
        // A window smaller than the chrome must yield a non-drawable rect, not
        // an underflow panic (both dimensions saturate independently).
        let expanded = ChromeState {
            panel_expanded: true,
            drawer_expanded: true,
        };
        let rect = ViewportRect::for_chrome(100, 100, &expanded);
        assert_eq!(rect.width, 0);
        assert_eq!(rect.height, 0);
        assert!(!rect.is_drawable());

        // A window wide enough but too short: width survives, height saturates.
        let rect = ViewportRect::for_chrome(1280, 50, &ChromeState::default());
        assert_eq!(rect.width, 1280 - 40);
        assert_eq!(rect.height, 0);
        assert!(!rect.is_drawable());
    }

    #[test]
    fn chrome_state_defaults_are_collapsed() {
        // UI-SPEC fixes the collapsed default for both the panel and the drawer.
        let chrome = ChromeState::default();
        assert!(!chrome.panel_expanded);
        assert!(!chrome.drawer_expanded);
        assert_eq!(chrome.panel_width(), CONTROLS_PANEL_COLLAPSED_WIDTH);
        assert_eq!(chrome.drawer_height(), 0);
    }

    #[test]
    fn surface_error_classifies_each_variant_for_recovery() {
        // FOUND-05: the frame path must branch per `SurfaceError` variant.
        // Outdated/Lost require recovery; Timeout/OutOfMemory/Other skip only.
        use reco_core::wgpu::SurfaceError;
        assert_eq!(
            classify_surface_error(&SurfaceError::Outdated),
            SurfaceErrorKind::Outdated
        );
        assert_eq!(
            classify_surface_error(&SurfaceError::Lost),
            SurfaceErrorKind::Lost
        );
        assert_eq!(
            classify_surface_error(&SurfaceError::Timeout),
            SurfaceErrorKind::Timeout
        );
        assert_eq!(
            classify_surface_error(&SurfaceError::OutOfMemory),
            SurfaceErrorKind::OutOfMemory
        );
        assert_eq!(
            classify_surface_error(&SurfaceError::Other),
            SurfaceErrorKind::Other
        );
    }

    #[test]
    fn recovery_action_distinguishes_rebuild_from_skip() {
        // Outdated/Lost are recovery actions (reconfigure / rebuild); the
        // transient variants are plain skips. Asserting the classification here
        // pins the contract the worker's frame path branches on.
        let is_recovery = |kind: SurfaceErrorKind| {
            matches!(kind, SurfaceErrorKind::Outdated | SurfaceErrorKind::Lost)
        };
        assert!(is_recovery(SurfaceErrorKind::Outdated));
        assert!(is_recovery(SurfaceErrorKind::Lost));
        assert!(!is_recovery(SurfaceErrorKind::Timeout));
        assert!(!is_recovery(SurfaceErrorKind::OutOfMemory));
        assert!(!is_recovery(SurfaceErrorKind::Other));
    }

    #[test]
    fn idle_clear_color_is_the_app_ground() {
        // UI-SPEC E3: the idle frame is the documented ground, not an
        // arbitrary placeholder.
        assert_eq!(
            IDLE_CLEAR_COLOR,
            [30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 1.0]
        );
    }

    #[test]
    fn presenter_kind_serializes_snake_case_and_carries_locked_labels() {
        // The typed command/event vocabulary (T-02-07) and the locked UI-SPEC
        // badge labels.
        assert_eq!(
            serde_json::to_string(&PresenterKind::Native).unwrap(),
            "\"native\""
        );
        assert_eq!(
            serde_json::to_string(&PresenterKind::SeparateWindow).unwrap(),
            "\"separate_window\""
        );
        assert_eq!(
            serde_json::to_string(&PresenterKind::Readback).unwrap(),
            "\"readback\""
        );
        assert_eq!(PresenterKind::Native.label(), "Presenter: Native");
        assert_eq!(
            PresenterKind::SeparateWindow.label(),
            "Presenter: Separate window"
        );
        assert_eq!(PresenterKind::Readback.label(), "Presenter: Readback");
    }

    #[test]
    fn chain_is_ordered_strongest_first() {
        assert_eq!(
            PRESENTER_CHAIN,
            [
                PresenterKind::Native,
                PresenterKind::SeparateWindow,
                PresenterKind::Readback
            ]
        );
    }

    #[test]
    fn is_fallthrough_covers_the_four_construction_errors() {
        let u = PresenterError::Unsupported { reason: "x".into() };
        let c = PresenterError::ChildView { reason: "x".into() };
        let s = PresenterError::Surface { reason: "x".into() };
        let w = PresenterError::Window { reason: "x".into() };
        assert!(is_fallthrough(&u));
        assert!(is_fallthrough(&c));
        assert!(is_fallthrough(&s));
        assert!(is_fallthrough(&w));
        // Runtime-state variants never occur during construction.
        assert!(!is_fallthrough(&PresenterError::NotConfigured));
        assert!(!is_fallthrough(&PresenterError::SurfaceLost {
            kind: SurfaceErrorKind::Lost
        }));
    }

    #[test]
    fn choose_presenter_returns_first_success() {
        let attempts = [
            (
                PresenterKind::Native,
                Err(PresenterError::Unsupported {
                    reason: "wayland".into(),
                }),
            ),
            (PresenterKind::SeparateWindow, Ok(())),
            (PresenterKind::Readback, Ok(())),
        ];
        let (kind, reason) = choose_presenter(&attempts);
        assert_eq!(kind, PresenterKind::SeparateWindow);
        assert!(reason.is_none());
    }

    #[test]
    fn choose_presenter_reports_weakest_with_reason_when_all_fail() {
        let attempts = [
            (
                PresenterKind::Native,
                Err(PresenterError::Unsupported {
                    reason: "wayland".into(),
                }),
            ),
            (
                PresenterKind::SeparateWindow,
                Err(PresenterError::Window {
                    reason: "no window".into(),
                }),
            ),
            (
                PresenterKind::Readback,
                Err(PresenterError::Surface {
                    reason: "no device".into(),
                }),
            ),
        ];
        let (kind, reason) = choose_presenter(&attempts);
        assert_eq!(kind, PresenterKind::Readback);
        assert!(matches!(reason, Some(PresenterError::Surface { .. })));
    }

    #[test]
    fn fallback_warn_line_matches_the_locked_shape() {
        let line = fallback_warn_line(
            PresenterKind::Readback,
            "native compositing unavailable on this display server (Wayland)",
        );
        assert_eq!(
            line,
            "Presenter fallback to Readback: native compositing unavailable on this display \
             server (Wayland). Preview is throttled. Use a separate preview window for smoother \
             playback."
        );
        // Exactly one trailing period after the reason (punct trimmed once).
        assert!(!line.contains(".."));
    }
}
