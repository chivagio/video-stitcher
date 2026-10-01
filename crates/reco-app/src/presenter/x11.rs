//! X11 native child-view presenter (D-01).
//!
//! Creates an X11 child window from the parent Xlib window id obtained from the
//! Tauri/tao window, then builds a `wgpu::Surface` on that child handle using
//! the **worker's** shared device/queue (D-03).
//!
//! All raw-handle access stays inside this module: the parent handle is turned
//! into a child window id here and never escapes (D-02).
//!
//! # Why a child window
//!
//! The webview leaves the top region uncovered (UI-SPEC). This presenter
//! creates an X11 child window covering that region so the `wgpu::Surface`
//! renders into a native layer *under* the webview. See [`super`] for the
//! two-layer model and D-01/D-02/D-03.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};
use reco_core::render::stitch_renderer::StitchRenderer;
use reco_core::source::YuvData;
use x11_dl::xlib;

use super::{PresenterError, SurfacePresenter, ViewportRect};

/// X11-backed panorama presenter.
///
/// Owns the child `Window` id, the `wl` display connection used to create it,
/// and the [`reco_core::wgpu::Surface`] built on the child handle.
pub struct X11Presenter {
    /// The X11 child window id used as the surface target.
    child_window: std::num::NonZeroU64,
    /// Kept alive so the child window outlives the surface (wgpu requires the
    /// window to outlive the surface).
    xlib: xlib::Xlib,
    /// The X connection the child window belongs to. Closed on drop *after*
    /// the surface (field order = drop order).
    display: *mut xlib::Display,
    /// The wgpu surface built on the child window.
    surface: reco_core::wgpu::Surface<'static>,
    /// The child window's current geometry.
    viewport: ViewportRect,
    /// The surface texture format chosen during `configure`.
    surface_format: Option<reco_core::wgpu::TextureFormat>,
    /// Supported alpha compositing modes for this surface.
    alpha_mode: reco_core::wgpu::CompositeAlphaMode,
}

// SAFETY: the Xlib `Display*` is only touched from the thread that creates the
// presenter; the presenter is moved to (and driven by) the engine worker
// thread, which owns the surface for its whole lifetime. Xlib requires that a
// connection not be used concurrently from multiple threads (`XInitThreads` is
// not called), and this type is never shared across threads.
unsafe impl Send for X11Presenter {}

impl X11Presenter {
    /// Create a presenter by building an X11 child window under `parent`
    /// (obtained from the Tauri window) and a wgpu surface on it.
    ///
    /// # Errors
    ///
    /// * [`PresenterError::Unsupported`] if the parent window handle is not an
    ///   Xlib/Xcb handle (e.g. the app is running as a native Wayland client,
    ///   D-05 — no X11-style child embedding exists there).
    /// * [`PresenterError::ChildView`] if the parent window/display handle is
    ///   unusable or the child window cannot be created.
    /// * [`PresenterError::Surface`] if the wgpu surface cannot be built.
    pub fn new<W>(
        window: &W,
        instance: &reco_core::wgpu::Instance,
        rect: ViewportRect,
    ) -> Result<Self, PresenterError>
    where
        W: HasWindowHandle + HasDisplayHandle,
    {
        let window_handle = window.window_handle().map_err(|e| PresenterError::ChildView {
            reason: format!("window_handle() failed: {e:?}"),
        })?;
        let display_handle = window.display_handle().map_err(|e| PresenterError::ChildView {
            reason: format!("display_handle() failed: {e:?}"),
        })?;

        // Extract the parent Xlib window id and its X connection. A Wayland
        // parent has neither — that is the D-05 unsupported path, not a bug.
        //
        // The child MUST be created on the same X connection that owns the
        // parent window; a fresh XOpenDisplay would produce a window the parent
        // server-side window cannot contain.
        let (parent_xid, display) = match window_handle.as_raw() {
            RawWindowHandle::Xlib(h) => {
                if h.display.is_null() {
                    return Err(PresenterError::ChildView {
                        reason: "Xlib window handle had a null display".to_string(),
                    });
                }
                (h.window, h.display)
            }
            other => {
                return Err(PresenterError::Unsupported {
                    reason: format!(
                        "parent window handle is {other:?}, not Xlib — \
                         Wayland has no X11-style child embedding (D-05)"
                    ),
                });
            }
        };

        // SAFETY: `display` is a non-null pointer taken from the parent window
        // handle; it is the live connection that owns `parent_xid` and remains
        // valid for as long as that window exists (the Tauri window outlives
        // this presenter in `setup`). `XDefaultScreen`/`XRootWindow` are queries
        // on that connection.
        let xlib = xlib::Xlib::open().map_err(|e| PresenterError::ChildView {
            reason: format!("failed to load libX11: {e}"),
        })?;

        // SAFETY: `display` is non-null and valid (checked above). The window
        // geometry is sanitized to at least 1x1. `parent_xid` is the parent
        // server-side window, so the created window is a true X11 child.
        let child_window = unsafe {
            (xlib.XCreateSimpleWindow)(
                display,
                parent_xid,
                0,
                0,
                rect.width.max(1),
                rect.height.max(1),
                0,
                0,
                0,
            )
        };
        if child_window == 0 {
            return Err(PresenterError::ChildView {
                reason: "XCreateSimpleWindow returned 0".to_string(),
            });
        }

        // SAFETY: `display` and `child_window` are valid; XMapWindow shows the
        // child so the surface is visible, XFlush pushes both requests.
        unsafe {
            (xlib.XMapWindow)(display, child_window);
            (xlib.XFlush)(display);
        }

        let child_nonzero = std::num::NonZeroU64::new(child_window).ok_or_else(|| {
            PresenterError::ChildView {
                reason: "child window id was zero".to_string(),
            }
        })?;

        // SAFETY: the child window id is a valid X11 window for the lifetime of
        // this presenter (destroyed only in Drop), and the display connection is
        // owned by the parent window which outlives the surface. wgpu's unsafe
        // contract is that the window outlives the surface — Drop order
        // guarantees this (the surface is dropped before the child window is
        // destroyed). We reuse the parent's raw display handle verbatim so the
        // surface is bound to the same connection as its window.
        let surface = unsafe {
            let target = reco_core::wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: display_handle.as_raw(),
                raw_window_handle: RawWindowHandle::Xlib(raw_window_handle::XlibWindowHandle::new(
                    child_window,
                )),
            };
            instance
                .create_surface_unsafe(target)
                .map_err(|e| PresenterError::Surface {
                    reason: format!("create_surface_unsafe failed: {e}"),
                })?
        };

        Ok(Self {
            child_window: child_nonzero,
            xlib,
            display,
            surface,
            viewport: rect,
            surface_format: None,
            alpha_mode: reco_core::wgpu::CompositeAlphaMode::Auto,
        })
    }
}

impl SurfacePresenter for X11Presenter {
    fn configure(
        &mut self,
        device: &reco_core::wgpu::Device,
        adapter: &reco_core::wgpu::Adapter,
        width: u32,
        height: u32,
    ) -> Result<(), PresenterError> {
        let caps = self.surface.get_capabilities(adapter);
        let surface_format = caps
            .formats
            .first()
            .copied()
            .ok_or_else(|| PresenterError::Surface {
                reason: "surface reported no supported formats".to_string(),
            })?;

        // Strip sRGB from the surface format and carry the stripped format in
        // `view_formats` to avoid double-gamma — the same contract the CLI
        // preview uses (crates/reco-cli/src/preview.rs:466-486).
        let render_format = StitchRenderer::strip_srgb(surface_format);
        let view_formats = if render_format != surface_format {
            vec![render_format]
        } else {
            vec![]
        };

        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(reco_core::wgpu::CompositeAlphaMode::Auto);

        self.surface.configure(
            device,
            &reco_core::wgpu::SurfaceConfiguration {
                usage: reco_core::wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: surface_format,
                width: width.max(1),
                height: height.max(1),
                present_mode: reco_core::wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode,
                view_formats,
            },
        );
        self.surface_format = Some(surface_format);
        self.alpha_mode = alpha_mode;
        self.viewport = ViewportRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        Ok(())
    }

    fn render_frame(
        &mut self,
        renderer: &StitchRenderer,
        left: &YuvData,
        right: &YuvData,
        yaw: f32,
        pitch: f32,
    ) -> Result<(), PresenterError> {
        let surface_format = self.surface_format.ok_or(PresenterError::NotConfigured)?;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                return Err(PresenterError::Surface {
                    reason: format!("get_current_texture failed: {e:?}"),
                });
            }
        };
        let render_format = StitchRenderer::strip_srgb(surface_format);
        let view = frame
            .texture
            .create_view(&reco_core::wgpu::TextureViewDescriptor {
                format: Some(render_format),
                ..Default::default()
            });

        let left_planes = left.as_planes();
        let right_planes = right.as_planes();
        renderer
            .render_yuv(&left_planes, &right_planes, yaw, pitch, &view)
            .map_err(|e| PresenterError::Surface {
                reason: format!("render_yuv failed: {e}"),
            })?;

        // Present, then the frame is dropped before any future reconfigure
        // (wgpu panics if a SurfaceTexture is alive during configure).
        frame.present();
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), PresenterError> {
        // SAFETY: `display` and `child_window` are valid for this presenter's
        // lifetime; XResizeWindow is safe to call with them.
        unsafe {
            (self.xlib.XResizeWindow)(
                self.display,
                self.child_window.get(),
                width.max(1),
                height.max(1),
            );
            (self.xlib.XFlush)(self.display);
        }
        self.viewport = ViewportRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        Ok(())
    }

    fn viewport(&self) -> ViewportRect {
        self.viewport
    }
}

impl X11Presenter {
    /// Borrow the wgpu surface for device creation.
    ///
    /// Used by the binary only, to pass `&Surface` into
    /// `GpuContext::for_surface` (adapter selection compatible with this
    /// surface). The surface never leaves the Rust process and is never
    /// exposed across IPC (D-02).
    pub fn surface(&self) -> &reco_core::wgpu::Surface<'static> {
        &self.surface
    }
}

impl Drop for X11Presenter {
    fn drop(&mut self) {
        // Drop order: the surface is dropped (implicitly, field order) before
        // this runs, so the window outlives the surface (wgpu requires this).
        // We destroy only the child window; the X display connection is owned
        // by the parent (Tauri) window and must NOT be closed here.
        // SAFETY: `display` and `child_window` are valid for this presenter's
        // lifetime; XDestroyWindow on a window we created is safe. No other
        // thread touches this connection.
        unsafe {
            (self.xlib.XDestroyWindow)(self.display, self.child_window.get());
        }
    }
}
