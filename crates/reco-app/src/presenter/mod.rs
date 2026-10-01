//! Rust-owned surface presenter (D-02).
//!
//! # The two-layer model
//!
//! Phase 1 composites two independent layers inside **one** OS window
//! (CONTEXT D-01/D-02/D-03):
//!
//! ```text
//!   one Tauri/tao window
//!   ├── webview chrome (bottom-anchored: log pane 160px + control row 48px)
//!   └── native child view (top region) ── owns a wgpu::Surface
//!                                         render target = stitched panorama
//! ```
//!
//! The webview leaves the top region uncovered (UI-SPEC Surface Layout
//! Contract); the presenter renders the panorama into a **native child view**
//! positioned in that region. The webview and the panorama are independent
//! layers — the frontend never sizes, moves, or creates the native view, and
//! no raw handle (`raw-window-handle` / HWND / NSView / X11) is ever exposed
//! across IPC (D-02).
//!
//! # Device sharing, not device ownership
//!
//! D-03: the presenter owns its own [`wgpu::Surface`] but **shares the engine
//! worker's device and queue**. The engine worker remains the single device
//! owner (FOUND-03); the presenter is a render target the worker draws into.
//! The presenter therefore never calls [`GpuContext::for_surface`] (which would
//! mint its own device) — it borrows `device()` / `queue()` from the worker's
//! [`GpuContext`] and negotiates surface capabilities against the worker's
//! retained adapter.
//!
//! The trait surface ([`SurfacePresenter`]) is the seam Phase 2's
//! runtime-swappable presenter (PREV-05: native compositing → separate preview
//! window → throttled readback) extends.

use reco_core::gpu::GpuContext;
use reco_core::render::stitch_renderer::StitchRenderer;
use reco_core::render::viewport::ViewportConfig;
use reco_core::source::YuvData;

#[cfg(all(unix, not(target_os = "macos")))]
pub mod x11;

pub mod fallback;

/// The platform-native presenter selected at compile time.
///
/// `#[cfg]`-gated by target — never a runtime `match target_os`
/// (CONVENTIONS.md / ARCHITECTURE.md). Windows/macOS arm in as those impls
/// land; targets without a native child-view impl fall back to
/// [`fallback::FallbackPresenter`] (D-05).
#[cfg(all(unix, not(target_os = "macos")))]
pub type PlatformPresenter = x11::X11Presenter;

/// The platform-native presenter on targets with no native child-view impl.
#[cfg(not(all(unix, not(target_os = "macos"))))]
pub type PlatformPresenter = fallback::FallbackPresenter;

/// Height in logical pixels of the webview control row (UI-SPEC).
pub const CONTROL_ROW_HEIGHT: u32 = 48;
/// Height in logical pixels of the webview event/status log pane (UI-SPEC).
pub const LOG_PANE_HEIGHT: u32 = 160;

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

    /// Building the `wgpu::Surface` from the child handle failed.
    #[error("surface creation failed: {reason}")]
    Surface {
        /// Human-readable reason.
        reason: String,
    },

    /// The presenter had no surface configured yet.
    #[error("presenter is not configured")]
    NotConfigured,
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
    /// physical pixels.
    ///
    /// Reserves [`LOG_PANE_HEIGHT`] + [`CONTROL_ROW_HEIGHT`] at the bottom for
    /// the webview chrome. Saturates to zero rather than underflowing when the
    /// window is smaller than the chrome (a minimum window size is enforced by
    /// `tauri.conf.json`, but the computation must still be total).
    pub fn for_window(width: u32, height: u32) -> Self {
        let reserved = LOG_PANE_HEIGHT + CONTROL_ROW_HEIGHT;
        Self {
            x: 0,
            y: 0,
            width,
            height: height.saturating_sub(reserved),
        }
    }

    /// Whether the viewport has a drawable (non-zero) area.
    pub fn is_drawable(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// A render target owned by Rust for the stitched panorama (D-02).
///
/// Implementors hide all platform-specific child-view and raw-handle plumbing;
/// callers (the engine worker) only see configure / render / resize.
pub trait SurfacePresenter {
    /// Configure (or reconfigure) the surface against the shared device.
    ///
    /// Must be called with the **worker's** device and retained adapter — never
    /// a freshly-created second device or adapter (D-03).
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::Surface`] if the surface cannot be configured,
    /// or [`PresenterError::NotConfigured`] if no platform surface exists.
    fn configure(
        &mut self,
        device: &reco_core::wgpu::Device,
        adapter: &reco_core::wgpu::Adapter,
        width: u32,
        height: u32,
    ) -> Result<(), PresenterError>;

    /// Render one stitched frame into this presenter's surface.
    ///
    /// `left` / `right` are the decoded YUV420P planes and `yaw` / `pitch`
    /// position the panorama. Implementations acquire the next surface texture,
    /// render through the shared [`StitchRenderer`], and present it.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::NotConfigured`] if the surface is not
    /// configured, or [`PresenterError::Surface`] on a recoverable surface
    /// error (the caller decides whether to reconfigure).
    fn render_frame(
        &mut self,
        renderer: &StitchRenderer,
        left: &YuvData,
        right: &YuvData,
        yaw: f32,
        pitch: f32,
    ) -> Result<(), PresenterError>;

    /// Resize the presenter's surface to `width` × `height`.
    ///
    /// # Errors
    ///
    /// Returns [`PresenterError::Surface`] if the surface cannot be
    /// reconfigured.
    fn resize(&mut self, width: u32, height: u32) -> Result<(), PresenterError>;

    /// The current viewport geometry.
    fn viewport(&self) -> ViewportRect;
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
    fn viewport_height_is_window_minus_chrome() {
        let rect = ViewportRect::for_window(1280, 800);
        assert_eq!(rect.width, 1280);
        assert_eq!(rect.height, 800 - 208);
        assert_eq!(rect.x, 0);
        assert_eq!(rect.y, 0);
    }

    #[test]
    fn viewport_saturates_on_tiny_window() {
        let rect = ViewportRect::for_window(100, 100);
        assert_eq!(rect.width, 100);
        assert_eq!(rect.height, 0);
        assert!(!rect.is_drawable());
    }
}
