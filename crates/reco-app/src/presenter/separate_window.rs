//! Separate-window presenter (PREV-05, chain step 2).
//!
//! Renders the stitched panorama into a **Rust-owned, webview-less Tauri
//! window** (`label = "preview"`) instead of a native child view embedded under
//! the chrome webview. This is the zero-copy fallback for platforms where the
//! native child-view arm is not implemented (Windows/macOS in Phase 2) or where
//! the display server cannot embed a child surface (a native Wayland session),
//! and it is the operator-selectable "Separate window" presenter.
//!
//! # Why a Tauri window and not a raw OS window
//!
//! One windowing stack: `tauri::window::Window` already implements
//! `raw_window_handle::{HasWindowHandle, HasDisplayHandle}` (and `Clone +
//! 'static`), so `instance.create_surface(window.clone())` works through the
//! ordinary wgpu `SurfaceTarget::Window` path. A raw winit/OS window would
//! duplicate Tauri's event-loop ownership for no gain (RESEARCH "Don't
//! Hand-Roll").
//!
//! # Setup-thread construction (RESEARCH Pattern 5, Pitfall 5)
//!
//! The window **and** the surface are created on the Tauri setup thread, then
//! the whole presenter is moved to the worker. wgpu panics on non-main-thread
//! surface creation on macOS/Metal, and Tauri documents a Windows
//! window-creation deadlock when a window is built synchronously off the setup
//! thread. `main.rs` pre-creates the hidden `"preview"` window and hands it in.
//!
//! # Contract
//!
//! Implements the full [`SurfacePresenter`] contract by mirroring
//! [`super::x11::X11Presenter`]: sRGB-strip / `view_formats` configure idiom,
//! acquire + classify + present frame, idle clear pass, resize. It owns only
//! its surface and **shares the worker's device** (D-03) — never a second
//! device.

use reco_core::render::stitch_renderer::StitchRenderer;
use reco_core::source::YuvData;

use super::{FrameOutcome, PresenterError, SurfacePresenter, ViewportRect};

/// The label of the Rust-owned preview window (one per process).
pub const PREVIEW_WINDOW_LABEL: &str = "preview";

/// Panorama presenter that renders into a separate, webview-less Tauri window.
pub struct SeparateWindowPresenter {
    /// The Rust-owned preview window. Retained (not just used to build the
    /// surface) so `show`/`hide` and teardown can drive it; the window must
    /// outlive the surface (wgpu's unsafe contract), and field order guarantees
    /// it.
    window: Option<tauri::window::Window>,
    /// The wgpu surface built on `window` via the ordinary `SurfaceTarget::Window`
    /// path.
    surface: Option<reco_core::wgpu::Surface<'static>>,
    /// The child window's current geometry.
    viewport: ViewportRect,
    /// The surface texture format chosen during `configure`.
    surface_format: Option<reco_core::wgpu::TextureFormat>,
    /// Supported alpha compositing modes for this surface.
    alpha_mode: reco_core::wgpu::CompositeAlphaMode,
    /// The shared device/queue the surface was configured against (D-03), for
    /// the idle/clear pass. Cheap `Arc`-backed clones, re-set on every
    /// `configure`.
    device: Option<reco_core::wgpu::Device>,
    /// The shared command queue (see [`Self::device`]).
    queue: Option<reco_core::wgpu::Queue>,
}

impl SeparateWindowPresenter {
    /// Build a presenter around an already-created Tauri preview window and a
    /// wgpu `Instance` (both from the setup thread — see the module docs).
    ///
    /// # Errors
    ///
    /// * [`PresenterError::Window`] if the `wgpu::Surface` cannot be built from
    ///   the Tauri window handle.
    pub fn new(
        window: tauri::window::Window,
        instance: &reco_core::wgpu::Instance,
        rect: ViewportRect,
    ) -> Result<Self, PresenterError> {
        // `Window` is `Clone + 'static` and implements both raw-window-handle
        // traits, so `create_surface` handles it through the safe
        // `SurfaceTarget::Window` conversion (which also takes ownership of the
        // handle source so the surface outlives the window).
        let surface =
            instance
                .create_surface(window.clone())
                .map_err(|e| PresenterError::Window {
                    reason: format!("create_surface on preview window failed: {e}"),
                })?;
        Ok(Self {
            window: Some(window),
            surface: Some(surface),
            viewport: rect,
            surface_format: None,
            alpha_mode: reco_core::wgpu::CompositeAlphaMode::Auto,
            device: None,
            queue: None,
        })
    }

    /// The shared device this surface is configured against.
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

    /// The live surface, or a typed error once the window has been released.
    fn live_surface(&self) -> Result<&reco_core::wgpu::Surface<'static>, PresenterError> {
        self.surface.as_ref().ok_or(PresenterError::NotConfigured)
    }

    /// Show the preview window (the "Show preview window" action, PREV-05).
    ///
    /// A no-op after the window has been released.
    pub fn show(&self) -> Result<(), PresenterError> {
        let Some(window) = self.window.as_ref() else {
            return Ok(());
        };
        window.show().map_err(|e| PresenterError::Window {
            reason: format!("showing preview window failed: {e}"),
        })
    }

    /// Hide the preview window without destroying it.
    ///
    /// A no-op after the window has been released.
    pub fn hide(&self) -> Result<(), PresenterError> {
        let Some(window) = self.window.as_ref() else {
            return Ok(());
        };
        window.hide().map_err(|e| PresenterError::Window {
            reason: format!("hiding preview window failed: {e}"),
        })
    }
}

impl SurfacePresenter for SeparateWindowPresenter {
    fn surface(&self) -> Option<&reco_core::wgpu::Surface<'static>> {
        self.surface.as_ref()
    }

    fn rebind_instance(
        &mut self,
        instance: &reco_core::wgpu::Instance,
    ) -> Result<(), PresenterError> {
        // The Rust-owned window survives a device loss; only the Instance and
        // Surface are bound to the lost parent device. Recreate the surface from
        // the same window on the fresh instance.
        let window = self
            .window
            .as_ref()
            .ok_or_else(|| PresenterError::Surface {
                reason: "presenter surface rebound after its window was released".to_string(),
            })?;
        let surface =
            instance
                .create_surface(window.clone())
                .map_err(|e| PresenterError::Surface {
                    reason: format!("create_surface on rebind failed: {e}"),
                })?;
        self.surface = Some(surface);
        // Force the next `configure` to re-negotiate: capabilities are a
        // property of the (device, surface) pair, and the device changed.
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
        let surface = self.live_surface()?;
        let caps = surface.get_capabilities(adapter);
        let surface_format =
            caps.formats
                .first()
                .copied()
                .ok_or_else(|| PresenterError::Surface {
                    reason: "surface reported no supported formats".to_string(),
                })?;

        // Strip sRGB and carry the stripped format in `view_formats` to avoid
        // double-gamma — the same contract the X11 presenter and the CLI preview
        // use.
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

        surface.configure(
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
        let frame = match self.live_surface()?.get_current_texture() {
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

        renderer.pipeline_mut().set_fov(fov_degrees);
        let left_planes = left.as_planes();
        let right_planes = right.as_planes();
        renderer
            .render_yuv(&left_planes, &right_planes, yaw, pitch, &view)
            .map_err(|e| PresenterError::Surface {
                reason: format!("render_yuv failed: {e}"),
            })?;

        frame.present();
        Ok(FrameOutcome::Presented)
    }

    fn render_idle(&mut self) -> Result<(), PresenterError> {
        let surface_format = self.surface_format.ok_or(PresenterError::NotConfigured)?;
        let frame = match self.live_surface()?.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                let kind = super::classify_surface_error(&e);
                return match kind {
                    crate::presenter::SurfaceErrorKind::Outdated
                    | crate::presenter::SurfaceErrorKind::Lost => {
                        Err(PresenterError::SurfaceLost { kind })
                    }
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
                    label: Some("reco-separate-idle-clear"),
                });
        {
            let _pass = encoder.begin_render_pass(&reco_core::wgpu::RenderPassDescriptor {
                label: Some("reco-separate-idle-pass"),
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
        // The Tauri window itself is resized by its own window manager; the
        // presenter only tracks the geometry the worker will configure to. A
        // released window is a teardown-time no-op, not an error.
        self.viewport = rect;
        Ok(())
    }

    fn viewport(&self) -> ViewportRect {
        self.viewport
    }

    /// Hide the separate preview window now, while the app is still alive.
    ///
    /// Unlike the X11 presenter this owns a *top-level* Tauri window, not a raw
    /// child window, so teardown is a `hide` (Tauri owns the window's lifetime);
    /// dropping the handle is handled by [`Drop`]. Idempotent.
    fn release_presenter_window(&mut self) {
        if let Some(window) = self.window.as_ref() {
            let _ = window.hide();
        }
    }
}

impl Drop for SeparateWindowPresenter {
    fn drop(&mut self) {
        // Drop the surface before the window handle (field order also enforces
        // this): a `wgpu::Surface` must not outlive the window it was created
        // from.
        self.surface = None;
        self.window = None;
    }
}
