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

use super::{FrameOutcome, PresenterError, SurfacePresenter, ViewportRect};

/// Install an X error handler that logs and swallows the benign teardown errors
/// our foreign child window can provoke, instead of letting GDK abort.
///
/// The child window is created with raw Xlib, so GTK does not track it. During
/// GTK's own teardown the server may report an asynchronous error for a request
/// against that window (observed: `BadDrawable` / `request_code 14`
/// `GetGeometry`). GDK's default X error handler calls `exit()` — which would
/// kill the process before the engine's clean-stop path runs (FOUND-06). This
/// handler records the error at `debug` level and returns, so the app unwinds
/// normally.
///
/// Only *benign drawable* errors are swallowed; anything else is logged at
/// `error` level so a real protocol bug remains visible.
fn install_benign_x_error_handler(xlib: &xlib::Xlib, display: *mut xlib::Display) {
    /// X error code for `BadDrawable`.
    const BAD_DRAWABLE: u8 = 9;

    unsafe extern "C" fn handler(_: *mut xlib::Display, event: *mut xlib::XErrorEvent) -> i32 {
        // SAFETY: the X server guarantees a valid `XErrorEvent` pointer for the
        // duration of the callback; we only read its integer fields.
        let (code, request, minor) = unsafe {
            (
                (*event).error_code,
                (*event).request_code,
                (*event).minor_code,
            )
        };
        if code == BAD_DRAWABLE {
            log::debug!(
                "ignoring benign X BadDrawable during teardown (request_code={request} minor={minor})"
            );
        } else {
            log::error!("X error: code={code} request_code={request} minor={minor}");
        }
        // Returning 0 tells X the error is handled; the process is not aborted.
        0
    }

    // SAFETY: `display` is the live parent connection. `XSetErrorHandler` is
    // process-global for that connection; `handler` is a plain `extern "C"`
    // function that never unwinds.
    unsafe {
        (xlib.XSetErrorHandler)(Some(handler));
        // Flush so any pending error from presenter setup surfaces here rather
        // than during the first frame.
        (xlib.XSync)(display, 0);
    }
}

/// X11-backed panorama presenter.
///
/// Owns the child `Window` id, the `wl` display connection used to create it,
/// and the [`reco_core::wgpu::Surface`] built on the child handle.
pub struct X11Presenter {
    /// The X11 child window id used as the surface target.
    child_window: Option<std::num::NonZeroU64>,
    /// Kept alive so the child window outlives the surface (wgpu requires the
    /// window to outlive the surface).
    xlib: xlib::Xlib,
    /// The X connection the child window belongs to. Closed on drop *after*
    /// the surface (field order = drop order).
    display: *mut xlib::Display,
    /// The parent's raw display handle, retained so the surface can be rebuilt
    /// on a fresh `Instance` during `Lost` recovery (FOUND-05) without
    /// re-deriving it from the (possibly-gone) Tauri window handle.
    display_handle: raw_window_handle::RawDisplayHandle,
    /// The wgpu surface built on the child window.
    surface: reco_core::wgpu::Surface<'static>,
    /// The child window's current geometry.
    viewport: ViewportRect,
    /// The surface texture format chosen during `configure`.
    surface_format: Option<reco_core::wgpu::TextureFormat>,
    /// Supported alpha compositing modes for this surface.
    alpha_mode: reco_core::wgpu::CompositeAlphaMode,
    /// The shared device/queue the surface was configured against (D-03).
    ///
    /// Cheap `Arc`-backed clones; re-set on every `configure`, so a device
    /// rebuild (FOUND-05) updates them together with the surface. The presenter
    /// needs them to submit its own idle/clear pass (`render_idle`).
    device: Option<reco_core::wgpu::Device>,
    /// The shared command queue (see [`Self::device`]).
    queue: Option<reco_core::wgpu::Queue>,
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
        let window_handle = window
            .window_handle()
            .map_err(|e| PresenterError::ChildView {
                reason: format!("window_handle() failed: {e:?}"),
            })?;
        let display_handle = window
            .display_handle()
            .map_err(|e| PresenterError::ChildView {
                reason: format!("display_handle() failed: {e:?}"),
            })?;

        // Extract the parent Xlib window id from the window handle, and its X
        // connection from the *display* handle. raw-window-handle 0.6 splits
        // these: `XlibWindowHandle` is { window, visual_id } and carries no
        // display; `XlibDisplayHandle` is { display, screen }.
        //
        // A Wayland parent has neither — that is the D-05 unsupported path,
        // not a bug.
        //
        // The child MUST be created on the same X connection that owns the
        // parent window; a fresh XOpenDisplay would produce a window the parent
        // server-side window cannot contain.
        let parent_xid = match window_handle.as_raw() {
            RawWindowHandle::Xlib(h) => h.window,
            other => {
                return Err(PresenterError::Unsupported {
                    reason: format!(
                        "parent window handle is {other:?}, not Xlib — \
                         Wayland has no X11-style child embedding (D-05)"
                    ),
                });
            }
        };

        // The non-null `Display*` comes from the display handle. `Option<
        // NonNull<_>>` means "absent" (`None`) rather than a null pointer, so
        // there is no null check to perform here.
        let display = match display_handle.as_raw() {
            raw_window_handle::RawDisplayHandle::Xlib(h) => {
                h.display.ok_or_else(|| PresenterError::ChildView {
                    reason: "Xlib display handle carried no display pointer".to_string(),
                })?
            }
            other => {
                return Err(PresenterError::Unsupported {
                    reason: format!(
                        "parent display handle is {other:?}, not Xlib — \
                         Wayland has no X11-style child embedding (D-05)"
                    ),
                });
            }
        };
        let display = display.as_ptr().cast::<xlib::Display>();

        // SAFETY: `display` is a non-null pointer taken from the parent window
        // handle; it is the live connection that owns `parent_xid` and remains
        // valid for as long as that window exists (the Tauri window outlives
        // this presenter in `setup`). `XDefaultScreen`/`XRootWindow` are queries
        // on that connection.
        let xlib = xlib::Xlib::open().map_err(|e| PresenterError::ChildView {
            reason: format!("failed to load libX11: {e}"),
        })?;

        // Install an X error handler on the parent's connection. The child window
        // this presenter creates is deliberately *outside* GTK's knowledge, so
        // during GTK's own teardown the X server can deliver an asynchronous
        // error for a request against it (observed: `BadDrawable`, GetGeometry).
        // GDK's default handler treats any X error as fatal and aborts the
        // process before the engine's clean-stop path can run (FOUND-06), so we
        // swallow those during teardown and let the process unwind normally.
        install_benign_x_error_handler(&xlib, display);

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

        // Lower the child below any window added later (the chrome webview).
        // X11 stacks child windows above their parent; without this the native
        // child view (created before the webview, Pitfall 1) would sit above the
        // webview and swallow pointer events over the preview region (PREV-04).
        // See [`X11Presenter::lower`].
        Self::lower_window(&xlib, display, child_window);

        let child_nonzero =
            std::num::NonZeroU64::new(child_window).ok_or_else(|| PresenterError::ChildView {
                reason: "child window id was zero".to_string(),
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
            child_window: Some(child_nonzero),
            xlib,
            display,
            display_handle: display_handle.as_raw(),
            surface,
            viewport: rect,
            surface_format: None,
            alpha_mode: reco_core::wgpu::CompositeAlphaMode::Auto,
            device: None,
            queue: None,
        })
    }

    /// The shared device this surface is configured against.
    ///
    /// # Panics
    ///
    /// Never in practice: callers only reach this after a successful
    /// [`SurfacePresenter::configure`], which sets it. The `expect` documents
    /// the invariant rather than guessing.
    fn device(&self) -> &reco_core::wgpu::Device {
        self.device
            .as_ref()
            .expect("presenter device is set by configure()")
    }

    /// The shared command queue (see [`Self::device`]).
    fn queue(&self) -> &reco_core::wgpu::Queue {
        self.queue
            .as_ref()
            .expect("presenter queue is set by configure()")
    }

    /// Lower `child_window` to the bottom of its siblings on `display` (z-order).
    ///
    /// The X server stacks child windows in creation order (later = higher). The
    /// native child view is created *before* the chrome webview (Pitfall 1), so
    /// without this call it would sit **above** the webview and swallow pointer
    /// events over the preview region, breaking mouse-drag pan / wheel zoom
    /// (PREV-04). `XLowerWindow` places it beneath every sibling.
    ///
    /// Static helper so it is callable both during construction (before `Self`
    /// exists) and from [`Self::lower`]. A no-op for a zero window id.
    fn lower_window(xlib: &xlib::Xlib, display: *mut xlib::Display, child_window: u64) {
        if child_window == 0 {
            return;
        }
        // SAFETY: `display` is the live connection that owns `child_window`
        // (both come from the same parent connection), and `child_window` is a
        // valid X11 window for this presenter's lifetime.
        unsafe {
            (xlib.XLowerWindow)(display, child_window);
            (xlib.XFlush)(display);
        }
    }
}

impl X11Presenter {
    /// Lower the native child view below the chrome webview (z-order).
    ///
    /// Idempotent and safe to call at any point after construction; a no-op
    /// after [`X11Presenter::release_child_window`] has destroyed the window.
    /// `XLowerWindow(child, Below)` places the child beneath every sibling,
    /// which is what makes pointer events over the preview region reach the
    /// full-window webview (RESEARCH Pitfall 1).
    pub fn lower(&mut self) {
        let Some(child) = self.child_window else {
            return;
        };
        Self::lower_window(&self.xlib, self.display, child.get());
    }
}

impl SurfacePresenter for X11Presenter {
    fn surface(&self) -> Option<&reco_core::wgpu::Surface<'static>> {
        Some(&self.surface)
    }

    fn rebind_instance(
        &mut self,
        instance: &reco_core::wgpu::Instance,
    ) -> Result<(), PresenterError> {
        // The child window and its X connection survive a device loss; only the
        // Instance/Surface are bound to the lost parent device. Recreate the
        // surface from the same child window on the fresh instance.
        //
        // SAFETY: `child_window` is valid and owned by this presenter for its
        // whole lifetime (released only via `release_child_window`/`Drop`, after
        // the surface), and `display_handle` is the connection that owns it.
        // wgpu's unsafe contract is that the window outlives the surface; field
        // order guarantees it.
        let child_window = self.child_window.ok_or_else(|| PresenterError::Surface {
            reason: "presenter surface rebound after its window was destroyed".to_string(),
        })?;
        let surface = unsafe {
            let target = reco_core::wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: self.display_handle,
                raw_window_handle: RawWindowHandle::Xlib(raw_window_handle::XlibWindowHandle::new(
                    child_window.get(),
                )),
            };
            instance
                .create_surface_unsafe(target)
                .map_err(|e| PresenterError::Surface {
                    reason: format!("create_surface_unsafe on rebind failed: {e}"),
                })?
        };
        self.surface = surface;
        // Force the next `configure` to re-negotiate: capabilities are a property
        // of the (device, surface) pair, and the device changed.
        self.surface_format = None;
        self.device = None;
        self.queue = None;
        Ok(())
    }

    fn configure(
        &mut self,
        device: &reco_core::wgpu::Device,
        queue: &reco_core::wgpu::Queue,
        adapter: &reco_core::wgpu::Adapter,
        rect: ViewportRect,
    ) -> Result<(), PresenterError> {
        let caps = self.surface.get_capabilities(adapter);
        let surface_format =
            caps.formats
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
                width: rect.width.max(1),
                height: rect.height.max(1),
                present_mode: reco_core::wgpu::PresentMode::Fifo,
                desired_maximum_frame_latency: 2,
                alpha_mode,
                view_formats,
            },
        );
        self.surface_format = Some(surface_format);
        self.alpha_mode = alpha_mode;
        // Retain the shared device/queue alongside the surface so idle-frame
        // painting and any future internal pass submit on the SAME device the
        // surface is configured against (D-03). Re-set here on every configure,
        // so a FOUND-05 device rebuild updates them atomically with the surface.
        self.device = Some(device.clone());
        self.queue = Some(queue.clone());
        self.viewport = rect;
        Ok(())
    }

    fn render_frame(
        &mut self,
        renderer: &mut StitchRenderer,
        left: &YuvData,
        right: &YuvData,
        yaw: f32,
        pitch: f32,
        fov_degrees: f32,
    ) -> Result<FrameOutcome, PresenterError> {
        let surface_format = self.surface_format.ok_or(PresenterError::NotConfigured)?;
        // Acquire the frame. On a transient error we classify and skip; on
        // Outdated/Lost we return a recovery error. Critically, no
        // `SurfaceTexture` is ever held past this point — the frame (if any)
        // is dropped before the caller can reconfigure (Pitfall 3).
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                let kind = super::classify_surface_error(&e);
                return match kind {
                    // Recovery actions: the worker reconfigure/rebuilds before
                    // the next frame. No frame is alive here (acquire failed).
                    crate::presenter::SurfaceErrorKind::Outdated
                    | crate::presenter::SurfaceErrorKind::Lost => {
                        Err(PresenterError::SurfaceLost { kind })
                    }
                    // Transient: drop this frame and retry next tick.
                    crate::presenter::SurfaceErrorKind::Timeout
                    | crate::presenter::SurfaceErrorKind::OutOfMemory
                    | crate::presenter::SurfaceErrorKind::Other => {
                        Ok(FrameOutcome::Skipped { kind })
                    }
                };
            }
        };
        let render_format = StitchRenderer::strip_srgb(surface_format);
        let view = frame
            .texture
            .create_view(&reco_core::wgpu::TextureViewDescriptor {
                format: Some(render_format),
                ..Default::default()
            });

        // FOV is plumbed into the pipeline (which clamps 1..179 internally)
        // immediately before the draw, uniformly across every presenter impl
        // (PREV-04/PREV-05).
        renderer.pipeline_mut().set_fov(fov_degrees);
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
        Ok(FrameOutcome::Presented)
    }

    fn render_source(
        &mut self,
        renderer: &mut StitchRenderer,
        left: &YuvData,
        right: &YuvData,
    ) -> Result<FrameOutcome, PresenterError> {
        let surface_format = self.surface_format.ok_or(PresenterError::NotConfigured)?;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                let kind = super::classify_surface_error(&e);
                return match kind {
                    crate::presenter::SurfaceErrorKind::Outdated
                    | crate::presenter::SurfaceErrorKind::Lost => {
                        Err(PresenterError::SurfaceLost { kind })
                    }
                    crate::presenter::SurfaceErrorKind::Timeout
                    | crate::presenter::SurfaceErrorKind::OutOfMemory
                    | crate::presenter::SurfaceErrorKind::Other => {
                        Ok(FrameOutcome::Skipped { kind })
                    }
                };
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
            .render_source_tiles(&left_planes, &right_planes, &view)
            .map_err(|e| PresenterError::Surface {
                reason: format!("render_source_tiles failed: {e}"),
            })?;

        frame.present();
        Ok(FrameOutcome::Presented)
    }

    fn render_idle(&mut self) -> Result<(), PresenterError> {
        // UI-SPEC E3: paint the reserved region with the app ground colour
        // before the first stitched frame so it is never an undefined/black
        // hole at startup. A `LoadOp::Clear` render pass with no draw is the
        // thinnest way to do this — no shader, no vertex buffer.
        let surface_format = self.surface_format.ok_or(PresenterError::NotConfigured)?;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                let kind = super::classify_surface_error(&e);
                return match kind {
                    crate::presenter::SurfaceErrorKind::Outdated
                    | crate::presenter::SurfaceErrorKind::Lost => {
                        Err(PresenterError::SurfaceLost { kind })
                    }
                    // A transient failure at startup is not fatal: the region
                    // simply stays as it was; the first stitched frame paints
                    // it as soon as the surface recovers.
                    _ => Ok(()),
                };
            }
        };
        let render_format = StitchRenderer::strip_srgb(surface_format);
        let view = frame
            .texture
            .create_view(&reco_core::wgpu::TextureViewDescriptor {
                format: Some(render_format),
                ..Default::default()
            });
        let mut encoder =
            self.device()
                .create_command_encoder(&reco_core::wgpu::CommandEncoderDescriptor {
                    label: Some("reco-idle-clear"),
                });
        {
            let _pass = encoder.begin_render_pass(&reco_core::wgpu::RenderPassDescriptor {
                label: Some("reco-idle-pass"),
                color_attachments: &[Some(reco_core::wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: reco_core::wgpu::Operations {
                        load: reco_core::wgpu::LoadOp::Clear(reco_core::wgpu::Color {
                            r: super::IDLE_CLEAR_COLOR[0],
                            g: super::IDLE_CLEAR_COLOR[1],
                            b: super::IDLE_CLEAR_COLOR[2],
                            a: super::IDLE_CLEAR_COLOR[3],
                        }),
                        store: reco_core::wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue().submit([encoder.finish()]);
        frame.present();
        Ok(())
    }

    fn resize(&mut self, rect: ViewportRect) -> Result<(), PresenterError> {
        // After `release_child_window` there is no window to resize; that is a
        // teardown-time no-op, not an error.
        let Some(child) = self.child_window else {
            return Ok(());
        };
        // SAFETY: `display` and `child` are valid for this presenter's lifetime
        // (the window is released only in `release_child_window`/`Drop`);
        // XResizeWindow is safe to call with them.
        unsafe {
            (self.xlib.XResizeWindow)(
                self.display,
                child.get(),
                rect.width.max(1),
                rect.height.max(1),
            );
            (self.xlib.XFlush)(self.display);
        }
        self.viewport = rect;
        Ok(())
    }

    fn viewport(&self) -> ViewportRect {
        self.viewport
    }

    fn configured_format(&self) -> Option<reco_core::wgpu::TextureFormat> {
        self.surface_format
    }

    /// FOUND-06: destroy the child window while the parent is still alive.
    ///
    /// Dispatched by the worker's `shutdown()` through `dyn SurfacePresenter`.
    /// Releasing here — rather than relying on `Drop` — is the only ordering in
    /// which GTK still owns the parent, so `XDestroyWindow` cannot raise the
    /// fatal `BadDrawable` that would abort the process before a clean stop is
    /// reported. Idempotent via [`X11Presenter::release_child_window`].
    fn release_presenter_window(&mut self) {
        self.release_child_window();
    }
}

impl Drop for X11Presenter {
    fn drop(&mut self) {
        // Only destroy what we still own. If `close()` already ran (the normal
        // teardown path), skip: re-destroying a window whose parent has been
        // torn down by GTK raises a fatal `BadDrawable` X error that aborts the
        // process before the worker can report a clean stop (FOUND-06).
        let Some(child) = self.child_window.take() else {
            return;
        };
        // SAFETY: `display` and `child` were valid for this presenter's lifetime
        // and the parent (Tauri) window is still alive at this point on the
        // drop path. No other thread touches this connection.
        unsafe {
            (self.xlib.XDestroyWindow)(self.display, child.get());
        }
    }
}

impl X11Presenter {
    /// Destroy the child window now, while the parent Tauri window is still
    /// alive, and mark it as released so [`Drop`] does not touch it again.
    ///
    /// Called from the worker's `shutdown()` (FOUND-06) so teardown happens in
    /// a known-good order: stop the loop → drop the decode/renderer/surface →
    /// destroy the child window (parent still valid) → drop the device. Doing
    /// this only in `Drop` risks running after GTK has torn the parent down,
    /// which raises `BadDrawable` and kills the process before the clean-stop
    /// path can report.
    ///
    /// Idempotent: a second call is a no-op.
    pub fn release_child_window(&mut self) {
        let Some(child) = self.child_window.take() else {
            return;
        };
        // SAFETY: as in `Drop` — `display` and `child` are valid, and this runs
        // while the parent window still exists.
        unsafe {
            (self.xlib.XDestroyWindow)(self.display, child.get());
            (self.xlib.XFlush)(self.display);
        }
    }
}
