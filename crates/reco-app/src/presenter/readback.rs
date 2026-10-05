//! Readback presenter — the throttled, visibly-degraded fallback (PREV-05).
//!
//! This is the **last resort** in the presenter chain and the **only** mode
//! where frame pixels cross IPC. It has **no surface**: the worker creates a
//! headless device (`GpuContext::with_surface(None)`) and the presenter renders
//! through the renderer's own RGBA readback path
//! ([`StitchRenderer::render_and_readback_rgba`]), pushing copied frames to the
//! webview over a Tauri [`Channel`](tauri::ipc::Channel) where they are painted
//! onto a canvas. The webview owns the preview region in this mode and shows the
//! locked degraded banner + badge (UI-SPEC Presenter & Degradation Contract).
//!
//! # Throttle
//!
//! Readback copies frames GPU→CPU and then Rust→webview, so it is capped at
//! [`READBACK_FPS`] frames per second by wall-clock (independent of the clip
//! fps). A single throttle bounds the IPC volume (T-02-10); the worker never
//! blocks on the channel send.
//!
//! # Never a second device
//!
//! The presenter builds no `GpuContext` and no `RgbaReadback` of its own — it
//! drives the worker's shared [`StitchRenderer`] (D-03). The readback staging
//! buffers live inside the renderer.
//!
//! # Frame wire format
//!
//! Every frame pushed over the [`Channel`](tauri::ipc::Channel) is prefixed with
//! its own pixel geometry so the webview binds its canvas backing store to each
//! frame instead of assuming a fixed size:
//!
//! ```text
//! [width: u32 little-endian][height: u32 little-endian][RGBA bytes]
//! ```
//!
//! [`READBACK_HEADER_LEN`] is the header size (8) and the byte offset at which
//! the RGBA payload begins. The frontend mirrors both the length and the layout
//! in `PreviewSurface.svelte`.

use std::time::{Duration, Instant};

use reco_core::render::stitch_renderer::StitchRenderer;
use reco_core::source::YuvData;

use super::{FrameOutcome, PresenterError, SurfacePresenter, ViewportRect};

/// Readback frame-rate cap (frames per second). Surfaced in the WARN line so
/// the degradation is honest about its cadence.
pub const READBACK_FPS: u32 = 10;

/// Length in bytes of the per-frame IPC header: two little-endian `u32`s
/// (`width`, then `height`). The RGBA payload starts at this offset.
///
/// The frontend (`PreviewSurface.svelte`) mirrors this constant as
/// `READBACK_HEADER_LEN`; the two must agree on the wire format. The frontend's
/// runtime length guard (`byteLength == READBACK_HEADER_LEN + width * height *
/// 4`) fails closed — no paint — if they ever drift.
pub const READBACK_HEADER_LEN: usize = 8;

/// Prefix `bytes` with the frame geometry:
/// `[width: u32 LE][height: u32 LE][RGBA bytes]`.
///
/// The readback IPC frame carries its own dimensions so the webview can size
/// its canvas backing store from the frame rather than from a fixed constant.
/// Consumed by the readback `paintFrame` in `PreviewSurface.svelte`.
pub fn frame_with_header(bytes: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut framed = Vec::with_capacity(READBACK_HEADER_LEN + bytes.len());
    framed.extend_from_slice(&width.to_le_bytes());
    framed.extend_from_slice(&height.to_le_bytes());
    framed.extend_from_slice(bytes);
    framed
}

/// Headless presenter that renders to CPU pixels and pushes them over IPC.
pub struct ReadbackPresenter {
    /// The webview channel the frames are pushed to, once the UI attaches one.
    ///
    /// `None` until the UI calls `preview_attach_readback`; frames are rendered
    /// (to keep the transport advancing) but not sent until a channel exists.
    channel: Option<tauri::ipc::Channel<tauri::ipc::Response>>,
    /// Current geometry (tracked for the worker's reconfigure path; there is no
    /// surface to configure).
    viewport: ViewportRect,
    /// Wall-clock of the last frame actually sent, for the [`READBACK_FPS`]
    /// throttle.
    last_send: Option<Instant>,
    /// Drop count of frames elided by the throttle (diagnostic).
    throttled: u64,
}

impl ReadbackPresenter {
    /// Build a headless readback presenter for `rect`.
    pub fn new(rect: ViewportRect) -> Self {
        Self {
            channel: None,
            viewport: rect,
            last_send: None,
            throttled: 0,
        }
    }

    /// Attach (or replace) the webview channel frames are pushed to.
    ///
    /// Called by the thin `preview_attach_readback` Tauri command; the presenter
    /// itself never names an engine type.
    pub fn attach_channel(&mut self, channel: tauri::ipc::Channel<tauri::ipc::Response>) {
        self.channel = Some(channel);
        // A fresh attachment should send its first frame immediately rather than
        // waiting out a throttle window from a previous attachment.
        self.last_send = None;
    }

    /// Whether a webview channel is currently attached.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn has_channel(&self) -> bool {
        self.channel.is_some()
    }

    /// The throttle window between sent frames.
    fn send_interval() -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(READBACK_FPS.max(1)))
    }

    /// Whether a frame may be sent now, updating the throttle clock if so.
    fn should_send(&mut self) -> bool {
        let now = Instant::now();
        match self.last_send {
            Some(last) if now.duration_since(last) < Self::send_interval() => {
                self.throttled += 1;
                false
            }
            _ => {
                self.last_send = Some(now);
                true
            }
        }
    }
}

impl SurfacePresenter for ReadbackPresenter {
    fn surface(&self) -> Option<&reco_core::wgpu::Surface<'static>> {
        // Headless: the worker creates the device via
        // `GpuContext::with_surface(None)` for this presenter.
        None
    }

    fn rebind_instance(
        &mut self,
        _instance: &reco_core::wgpu::Instance,
    ) -> Result<(), PresenterError> {
        // No surface to rebind; the device is rebuilt by the worker and the
        // renderer is recreated against it, so there is nothing surface-bound
        // here to refresh.
        Ok(())
    }

    fn configure(
        &mut self,
        _device: &reco_core::wgpu::Device,
        _queue: &reco_core::wgpu::Queue,
        _adapter: &reco_core::wgpu::Adapter,
        rect: ViewportRect,
    ) -> Result<(), PresenterError> {
        // Headless: track geometry only. The renderer is built/configured by the
        // worker against the shared device.
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
        // Capture the frame geometry BEFORE the readback call. The returned
        // slice borrows `renderer` mutably for as long as `bytes` is live, so an
        // `&self` accessor cannot be called alongside it (E0502).
        // `readback_dimensions` returns an owned `Option<(u32, u32)>`, ending
        // the immutable borrow at this statement.
        let dims = renderer.readback_dimensions();
        // FOV is plumbed uniformly across impls (PREV-04); the pipeline clamps
        // 1..179 internally.
        renderer.pipeline_mut().set_fov(fov_degrees);
        let left_planes = left.as_planes();
        let right_planes = right.as_planes();
        // The renderer owns the triple-buffered `RgbaReadback`; `None` on the
        // first two calls during warmup, `Some` from the third onward. The
        // staging buffer is created on the first call and never reset, so `dims`
        // is `Some` on every tick that yields `Some(bytes)`.
        let rgba = renderer
            .render_and_readback_rgba(&left_planes, &right_planes, yaw, pitch)
            .map_err(|e| PresenterError::Surface {
                reason: format!("render_and_readback_rgba failed: {e}"),
            })?;
        if let Some(bytes) = rgba
            && self.should_send()
            && let Some(channel) = self.channel.as_ref()
        {
            // Never block the worker on the send: a closed webview just drops the
            // frame (T-02-10). The frame self-describes its geometry so the
            // webview sizes its canvas from the frame. If `dims` were somehow
            // `None` while bytes exist, skip rather than send a headerless frame.
            if let Some((width, height)) = dims {
                let payload = frame_with_header(bytes, width, height);
                let _ = channel.send(tauri::ipc::Response::new(payload));
            }
        }
        // The readback path has no swapchain; "presented" here means a frame was
        // produced (and, subject to the throttle, delivered) this tick.
        Ok(FrameOutcome::Presented)
    }

    fn render_source(
        &mut self,
        renderer: &mut StitchRenderer,
        left: &YuvData,
        right: &YuvData,
    ) -> Result<FrameOutcome, PresenterError> {
        // Capture geometry before the readback borrow (see `render_frame`).
        let dims = renderer.readback_dimensions();
        let left_planes = left.as_planes();
        let right_planes = right.as_planes();
        // Render the source tiles into the internal target and read back,
        // mirroring `render_frame`'s panorama readback path.
        let rgba = renderer
            .render_source_and_readback_rgba(&left_planes, &right_planes)
            .map_err(|e| PresenterError::Surface {
                reason: format!("render_source_and_readback_rgba failed: {e}"),
            })?;
        if let Some(bytes) = rgba
            && self.should_send()
            && let Some(channel) = self.channel.as_ref()
        {
            // Same self-describing header as the panorama path.
            if let Some((width, height)) = dims {
                let payload = frame_with_header(bytes, width, height);
                let _ = channel.send(tauri::ipc::Response::new(payload));
            }
        }
        Ok(FrameOutcome::Presented)
    }

    fn render_idle(&mut self) -> Result<(), PresenterError> {
        // No-op: the webview owns the region in readback mode and paints its
        // degraded banner/idle state itself; there is no surface to clear.
        Ok(())
    }

    fn resize(&mut self, rect: ViewportRect) -> Result<(), PresenterError> {
        self.viewport = rect;
        Ok(())
    }

    fn viewport(&self) -> ViewportRect {
        self.viewport
    }

    fn attach_readback_channel(&mut self, channel: tauri::ipc::Channel<tauri::ipc::Response>) {
        self.attach_channel(channel);
    }

    fn configured_format(&self) -> Option<reco_core::wgpu::TextureFormat> {
        // The readback path renders into the renderer's internal target, which
        // is created in `Rgba8Unorm`.
        Some(reco_core::wgpu::TextureFormat::Rgba8Unorm)
    }

    // `release_presenter_window` uses the default no-op: there is no window.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_fps_is_the_locked_cadence() {
        // The cadence is surfaced in the WARN line, so it is a contract value.
        assert_eq!(READBACK_FPS, 10);
        assert_eq!(
            ReadbackPresenter::send_interval(),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn readback_has_no_surface_and_tracks_geometry() {
        let mut p = ReadbackPresenter::new(ViewportRect {
            x: 0,
            y: 0,
            width: 1280,
            height: 728,
        });
        assert!(p.surface().is_none());
        assert!(!p.has_channel());
        p.resize(ViewportRect {
            x: 0,
            y: 0,
            width: 1024,
            height: 600,
        })
        .unwrap();
        assert_eq!(p.viewport().width, 1024);
        assert_eq!(p.viewport().height, 600);
    }

    #[test]
    fn throttle_elides_frames_inside_the_window() {
        let mut p = ReadbackPresenter::new(ViewportRect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        });
        // The first frame is always allowed; an immediate second is throttled.
        assert!(p.should_send());
        assert!(!p.should_send());
        assert_eq!(p.throttled, 1);
    }

    #[test]
    fn attaching_a_channel_marks_the_presenter_as_attached() {
        let mut p = ReadbackPresenter::new(ViewportRect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        });
        assert!(!p.has_channel());
        let channel = tauri::ipc::Channel::<tauri::ipc::Response>::new(|_body| Ok(()));
        p.attach_readback_channel(channel);
        assert!(p.has_channel());
    }

    #[test]
    fn readback_header_len_is_the_payload_offset() {
        // The wire layout is `[width: u32 LE][height: u32 LE][RGBA bytes]`, so
        // the two 4-byte fields put the payload at offset 8. PreviewSurface
        // .svelte mirrors this constant and its runtime length guard fails
        // closed if the two ever drift.
        assert_eq!(READBACK_HEADER_LEN, 8);
        let framed = frame_with_header(&[1, 2, 3, 4], 2, 1);
        assert_eq!(framed.len(), READBACK_HEADER_LEN + 4);
        assert_eq!(&framed[READBACK_HEADER_LEN..], &[1, 2, 3, 4]);
    }

    #[test]
    fn frame_header_is_eight_little_endian_bytes() {
        let framed = frame_with_header(&[], 1000, 728);
        assert_eq!(framed.len(), READBACK_HEADER_LEN);
        assert_eq!(&framed[0..4], &1000u32.to_le_bytes());
        assert_eq!(&framed[4..8], &728u32.to_le_bytes());
        // A concrete byte assertion pins endianness and field order, not just
        // a round-trip through the same encoder.
        assert_eq!(framed, vec![0xE8, 0x03, 0x00, 0x00, 0xD8, 0x02, 0x00, 0x00]);
    }

    #[test]
    fn frame_with_header_appends_the_payload_unchanged() {
        let payload: Vec<u8> = (0..32).collect();
        let framed = frame_with_header(&payload, 4, 2);
        assert_eq!(framed.len(), READBACK_HEADER_LEN + payload.len());
        assert_eq!(&framed[READBACK_HEADER_LEN..], payload.as_slice());
    }

    #[test]
    fn frame_header_encodes_the_dimensions_it_is_given() {
        // A hard-coded 1240x728 header would fail this: encoding a different
        // pair must change the bytes.
        let a = frame_with_header(&[], 1000, 728);
        let b = frame_with_header(&[], 1240, 728);
        assert_ne!(a, b);
        assert_eq!(&a[0..4], &1000u32.to_le_bytes());
        assert_eq!(&b[0..4], &1240u32.to_le_bytes());
        assert_eq!(&a[4..8], &b[4..8]);
    }
}
