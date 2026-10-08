//! Fallback presenter for targets with no native child-view embedding.
//!
//! # Why this module exists
//!
//! Wayland has **no X11-style child-window embedding**: a `wl_surface` belongs
//! to a top-level `xdg_toplevel`, and there is no supported way for one client
//! to embed a native child surface under another client's webview. Under a
//! native Wayland session the child-view path is expected to fail. Per CONTEXT
//! D-05 (a *recorded* verdict, not a silent downgrade), that FAIL is routed to
//! Phase 2's presenter fallback (separate preview window / throttled readback,
//! PREV-05) rather than worked around inside Phase 1.
//!
//! Following the project's "document, don't work around" rule (AGENTS.md), this
//! module is the explicit, typed posture: it **never panics** and returns
//! [`PresenterError::Unsupported`] with a non-empty reason so a Wayland-only
//! build still links and reports cleanly.
//!
//! This is the rationale analog of `crates/reco-gui/src/preview.rs:20-27`
//! ("documented 'why this design' module header").

use reco_core::render::stitch_renderer::StitchRenderer;
use reco_core::source::YuvData;

use super::{PresenterError, SurfacePresenter, ViewportRect, viewport_config};

/// Unsupported-target presenter.
///
/// Carries the recorded reason and a viewport for layout purposes; every
/// rendering operation returns [`PresenterError::Unsupported`].
// No target constructs this presenter any more: `main.rs::run_skeleton` is a
// single cross-platform function whose non-unix arm records the identical D-05
// posture as `native_error` instead. The type is kept as the typed D-05 record,
// is exercised by this module's unit tests on every target, and is allowed dead
// code everywhere so `cargo clippy -- -D warnings` stays clean on Windows too.
#[allow(dead_code)]
pub struct FallbackPresenter {
    reason: String,
    viewport: ViewportRect,
}

#[allow(dead_code)]
impl FallbackPresenter {
    /// Construct a fallback presenter carrying the recorded reason.
    pub fn new(reason: impl Into<String>, viewport: ViewportRect) -> Self {
        let reason = reason.into();
        // Guard the "non-empty reason" contract at the constructor boundary so
        // callers can never produce a context-free Unsupported error.
        let reason = if reason.trim().is_empty() {
            "native child-view compositing is unavailable on this target (D-05)".to_string()
        } else {
            reason
        };
        Self { reason, viewport }
    }

    /// The recorded reason this target is unsupported.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The viewport config for consumers that still want geometry.
    pub fn viewport_config(
        &self,
        blend_width: f32,
        rig_tilt: f32,
    ) -> reco_core::render::viewport::ViewportConfig {
        viewport_config(self.viewport, blend_width, rig_tilt)
    }

    /// The typed error every rendering operation returns on this target.
    ///
    /// GPU-free, so it is unit-testable without a device (the trait methods
    /// delegate here).
    pub fn unsupported(&self) -> PresenterError {
        PresenterError::Unsupported {
            reason: self.reason.clone(),
        }
    }
}

impl SurfacePresenter for FallbackPresenter {
    fn surface(&self) -> Option<&reco_core::wgpu::Surface<'static>> {
        None
    }

    fn rebind_instance(
        &mut self,
        _instance: &reco_core::wgpu::Instance,
    ) -> Result<(), PresenterError> {
        Err(self.unsupported())
    }

    fn configure(
        &mut self,
        _device: &reco_core::wgpu::Device,
        _queue: &reco_core::wgpu::Queue,
        _adapter: &reco_core::wgpu::Adapter,
        _rect: ViewportRect,
    ) -> Result<(), PresenterError> {
        Err(self.unsupported())
    }

    fn render_frame(
        &mut self,
        _renderer: &mut StitchRenderer,
        _left: &YuvData,
        _right: &YuvData,
        _yaw: f32,
        _pitch: f32,
        _fov_degrees: f32,
    ) -> Result<super::FrameOutcome, PresenterError> {
        Err(self.unsupported())
    }

    fn render_idle(&mut self) -> Result<(), PresenterError> {
        Err(self.unsupported())
    }

    fn resize(&mut self, rect: ViewportRect) -> Result<(), PresenterError> {
        self.viewport = rect;
        Ok(())
    }

    fn viewport(&self) -> ViewportRect {
        self.viewport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_returns_unsupported_with_non_empty_reason() {
        let mut presenter = FallbackPresenter::new(
            "Wayland has no child-window embedding",
            ViewportRect::for_chrome(1280, 800, &crate::presenter::ChromeState::default()),
        );
        // `resize` is allowed to succeed (it is layout only) ...
        assert!(
            presenter
                .resize(ViewportRect {
                    x: 0,
                    y: 0,
                    width: 1024,
                    height: 768
                })
                .is_ok()
        );
        assert_eq!(presenter.viewport().width, 1024);
        // ... but every rendering operation reports a typed `Unsupported`.
        assert!(presenter.render_idle().is_err());
        match presenter.unsupported() {
            PresenterError::Unsupported { reason } => {
                assert!(!reason.is_empty());
                assert!(reason.contains("Wayland"));
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn fallback_reason_defaults_when_blank() {
        let presenter = FallbackPresenter::new(
            "   ",
            ViewportRect::for_chrome(800, 600, &crate::presenter::ChromeState::default()),
        );
        assert!(!presenter.reason().trim().is_empty());
    }
}
