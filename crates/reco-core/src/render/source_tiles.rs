//! Raw source tile renderer (PREV-03).
//!
//! Renders the *left* and *right* decoded camera frames into two viewports of
//! a caller-provided [`wgpu::TextureView`] — no panorama stitching, no second
//! render target, no CPU readback. This is the additive `reco-core` API the
//! `reco-app` presenter needs to implement the source↔panorama comparison
//! toggle: no existing API draws two raw tiles into a *provided* view
//! ([`LensPreviewRenderer`](crate::lens::preview::LensPreviewRenderer) allocates
//! and returns its own texture and requires a whole [`GpuContext`](crate::gpu::GpuContext)).
//!
//! The gap and this resolution are recorded in
//! `crates/reco-app/FRICTION.md` (A2), per the project rule "document friction,
//! don't work around it".
//!
//! # Layout
//!
//! The target view is split into two equal halves side-by-side (left source on
//! the left, right source on the right), with a 1px accent separator column
//! between them. Each tile uses **contain** letterboxing: the source aspect is
//! preserved and the image is pillarboxed/letterboxed inside its half, never
//! stretched.
//!
//! # Device
//!
//! The renderer creates its pipeline and textures on the **caller-supplied**
//! device (the engine worker's shared device, D-03) and never allocates a second
//! device or adapter.
//!
//! # Example
//!
//! ```rust,ignore
//! use reco_core::render::source_tiles::SourceTileRenderer;
//!
//! let mut tiles = SourceTileRenderer::new(device, queue, format, w, h, tw, th)?;
//! tiles.draw(queue, &left_planes, &right_planes, &view)?;
//! ```

use wgpu::util::DeviceExt;

use super::pipeline::{PipelineError, YuvPlanes};

/// One vertex of a tile quad: clip-space position + texture coordinate.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct TileVertex {
    /// Clip-space position `[x, y]` in `[-1, 1]`.
    position: [f32; 2],
    /// Texture coordinate `[u, v]` in `[0, 1]`.
    uv: [f32; 2],
}

impl TileVertex {
    /// Vertex buffer layout: one `Float32x2` position + one `Float32x2` uv.
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<TileVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
        ],
    };
}

/// Compute the letterboxed quad (positions + uvs) for one tile.
///
/// `tile_ndc_x0`/`tile_ndc_x1` bound the tile's half in clip space
/// (`[-1, 1]`); the full `[-1, 1]` vertical range is used. The source aspect is
/// preserved inside that half: if the source is wider than the tile, the quad
/// spans the full tile width and shrinks vertically (letterbox); if it is
/// taller, it shrinks horizontally (pillarbox). The result is two triangles.
fn letterbox_quad(
    tile_ndc_x0: f32,
    tile_ndc_x1: f32,
    tile_w: f32,
    tile_h: f32,
    source_w: f32,
    source_h: f32,
) -> [TileVertex; 6] {
    let tile_aspect = (tile_w.max(1.0)) / (tile_h.max(1.0));
    let source_aspect = (source_w.max(1.0)) / (source_h.max(1.0));

    // Scale factors (1.0 = fills the tile in that axis).
    let (fit_w, fit_h) = if source_aspect >= tile_aspect {
        // Source is wider than the tile: fill width, letterbox vertically.
        (1.0, tile_aspect / source_aspect)
    } else {
        // Source is taller than the tile: fill height, pillarbox horizontally.
        (source_aspect / tile_aspect, 1.0)
    };

    let cx = (tile_ndc_x0 + tile_ndc_x1) * 0.5;
    let half_w = (tile_ndc_x1 - tile_ndc_x0) * 0.5 * fit_w;
    let half_h = fit_h; // the full vertical half-range is 1.0
    let x0 = cx - half_w;
    let x1 = cx + half_w;
    let y0 = -half_h;
    let y1 = half_h;

    // v=0 at the top (y1), matching the tile's top-left origin.
    [
        TileVertex {
            position: [x0, y0],
            uv: [0.0, 1.0],
        },
        TileVertex {
            position: [x1, y0],
            uv: [1.0, 1.0],
        },
        TileVertex {
            position: [x1, y1],
            uv: [1.0, 0.0],
        },
        TileVertex {
            position: [x0, y0],
            uv: [0.0, 1.0],
        },
        TileVertex {
            position: [x1, y1],
            uv: [1.0, 0.0],
        },
        TileVertex {
            position: [x0, y1],
            uv: [0.0, 0.0],
        },
    ]
}

/// Width of the accent separator column, in pixels (a 1px divider between the
/// two source tiles; the clear colour shows through it).
const SEPARATOR_WIDTH: f32 = 1.0;

/// Ground colour of the tile background (letterbox bars + the separator), the
/// same `#1e1e1e` app ground the presenters clear their region to.
const TILE_CLEAR_COLOR: [f64; 4] = [30.0 / 255.0, 30.0 / 255.0, 30.0 / 255.0, 1.0];

/// Renderer for two raw YUV420P source tiles drawn into a provided view.
pub struct SourceTileRenderer {
    /// The render pipeline, targeting whatever format the view uses.
    pipeline: wgpu::RenderPipeline,
    /// Quad geometry for the left tile (two triangles).
    left_buffer: wgpu::Buffer,
    /// Quad geometry for the right tile (two triangles).
    right_buffer: wgpu::Buffer,
    /// Y/U/V textures for the left source.
    left_y: wgpu::Texture,
    left_u: wgpu::Texture,
    left_v: wgpu::Texture,
    /// Y/U/V textures for the right source.
    right_y: wgpu::Texture,
    right_u: wgpu::Texture,
    right_v: wgpu::Texture,
    /// Filtering sampler shared by both tiles.
    sampler: wgpu::Sampler,
    /// The bind-group layout for a tile (3 plane textures + 1 sampler).
    texture_layout: wgpu::BindGroupLayout,
    /// The device, retained so `draw` can build per-frame bind groups.
    device: wgpu::Device,
    /// The view format the pipeline targets (part of the cache key).
    view_format: wgpu::TextureFormat,
    /// Source frame dimensions (both tiles share them).
    source_width: u32,
    source_height: u32,
    /// One tile's pixel size (part of the cache key; the viewport is split in
    /// half with a 1px separator).
    tile_width: u32,
    tile_height: u32,
}

impl SourceTileRenderer {
    /// Build a tile renderer for a view of `view_format`.
    ///
    /// `source_width`/`source_height` are the decoded frame dimensions (both
    /// cameras share them). `tile_width`/`tile_height` are the pixel size of a
    /// single tile's half of the view — used to compute the letterbox so the
    /// source aspect is preserved (never stretched).
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError::InvalidConfig`] when a dimension is zero or the
    /// source exceeds [`crate::calibration::MAX_DIM`].
    pub fn new(
        device: &wgpu::Device,
        view_format: wgpu::TextureFormat,
        source_width: u32,
        source_height: u32,
        tile_width: u32,
        tile_height: u32,
    ) -> Result<Self, PipelineError> {
        if source_width == 0 || source_height == 0 {
            return Err(PipelineError::InvalidConfig {
                reason: format!(
                    "source tile dimensions must be > 0, got {source_width}x{source_height}"
                ),
            });
        }
        if source_width > crate::calibration::MAX_DIM || source_height > crate::calibration::MAX_DIM
        {
            return Err(PipelineError::InvalidConfig {
                reason: format!(
                    "source dimensions {source_width}x{source_height} exceed MAX_DIM ({})",
                    crate::calibration::MAX_DIM
                ),
            });
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("source_tiles_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("source_tiles.wgsl").into()),
        });

        let tile_w = tile_width.max(1) as f32;
        let tile_h = tile_height.max(1) as f32;

        // Half-tile boundaries in clip space `[-1, 1]`. The view is two equal
        // halves; the separator is carved out of the inner edges.
        let sep_ndc = SEPARATOR_WIDTH / tile_w;
        let left_x1 = -sep_ndc * 0.5;
        let right_x0 = sep_ndc * 0.5;

        let sw = source_width as f32;
        let sh = source_height as f32;
        let left_quad = letterbox_quad(-1.0, left_x1, tile_w, tile_h, sw, sh);
        let right_quad = letterbox_quad(right_x0, 1.0, tile_w, tile_h, sw, sh);

        let left_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("source_tiles_left_quad"),
            contents: bytemuck::cast_slice(&left_quad),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let right_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("source_tiles_right_quad"),
            contents: bytemuck::cast_slice(&right_quad),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };

        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("source_tiles_tex_layout"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("source_tiles_layout"),
            bind_group_layouts: &[&texture_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("source_tiles_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[TileVertex::LAYOUT],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: view_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("source_tiles_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let create_tex = |label: &str, w: u32, h: u32| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage,
                view_formats: &[],
            })
        };

        let half_w = (source_width / 2).max(1);
        let half_h = (source_height / 2).max(1);
        let left_y = create_tex("source_tiles_left_y", source_width, source_height);
        let left_u = create_tex("source_tiles_left_u", half_w, half_h);
        let left_v = create_tex("source_tiles_left_v", half_w, half_h);
        let right_y = create_tex("source_tiles_right_y", source_width, source_height);
        let right_u = create_tex("source_tiles_right_u", half_w, half_h);
        let right_v = create_tex("source_tiles_right_v", half_w, half_h);

        Ok(Self {
            pipeline,
            left_buffer,
            right_buffer,
            left_y,
            left_u,
            left_v,
            right_y,
            right_u,
            right_v,
            sampler,
            texture_layout,
            device: device.clone(),
            view_format,
            source_width,
            source_height,
            tile_width: tile_width.max(1),
            tile_height: tile_height.max(1),
        })
    }

    /// Whether this renderer already matches the given view format and layout.
    ///
    /// Used by [`StitchRenderer::render_source_tiles`](super::stitch_renderer::StitchRenderer::render_source_tiles)
    /// to decide whether the cached tile renderer can be reused or must be
    /// rebuilt after a resize or a renderer swap (format change).
    pub fn matches(
        &self,
        view_format: wgpu::TextureFormat,
        source_width: u32,
        source_height: u32,
        tile_width: u32,
        tile_height: u32,
    ) -> bool {
        self.view_format == view_format
            && self.source_width == source_width
            && self.source_height == source_height
            && self.tile_width == tile_width.max(1)
            && self.tile_height == tile_height.max(1)
    }

    /// Upload `planes` into the given Y/U/V texture set (tightly packed).
    fn upload_planes(
        queue: &wgpu::Queue,
        y_tex: &wgpu::Texture,
        u_tex: &wgpu::Texture,
        v_tex: &wgpu::Texture,
        planes: &YuvPlanes<'_>,
        width: u32,
        height: u32,
    ) -> Result<(), PipelineError> {
        let expect_y = (width * height) as usize;
        let expect_uv = ((width / 2).max(1) * (height / 2).max(1)) as usize;
        if planes.y.len() < expect_y || planes.u.len() < expect_uv || planes.v.len() < expect_uv {
            return Err(PipelineError::InvalidConfig {
                reason: format!(
                    "source planes too small: need y>={expect_y} u>={expect_uv} v>={expect_uv}"
                ),
            });
        }
        upload_plane(queue, y_tex, planes.y, width, height);
        upload_plane(
            queue,
            u_tex,
            planes.u,
            (width / 2).max(1),
            (height / 2).max(1),
        );
        upload_plane(
            queue,
            v_tex,
            planes.v,
            (width / 2).max(1),
            (height / 2).max(1),
        );
        Ok(())
    }

    /// Build a bind group for one tile's plane textures.
    fn tile_bind_group(
        &self,
        y_tex: &wgpu::Texture,
        u_tex: &wgpu::Texture,
        v_tex: &wgpu::Texture,
        label: &str,
    ) -> wgpu::BindGroup {
        let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let u_view = u_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let v_view = v_tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&u_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&v_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    /// Draw the two raw source tiles into `view`.
    ///
    /// The view is cleared to the app ground colour, then the left source is
    /// drawn into the left half and the right source into the right half, each
    /// letterboxed (contain). A 1px accent separator is left between the halves
    /// (the clear colour shows through).
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError::InvalidConfig`] when a plane slice is too small
    /// for the declared dimensions.
    pub fn draw(
        &mut self,
        queue: &wgpu::Queue,
        left: &YuvPlanes<'_>,
        right: &YuvPlanes<'_>,
        view: &wgpu::TextureView,
    ) -> Result<(), PipelineError> {
        let w = self.source_width;
        let h = self.source_height;
        Self::upload_planes(queue, &self.left_y, &self.left_u, &self.left_v, left, w, h)?;
        Self::upload_planes(
            queue,
            &self.right_y,
            &self.right_u,
            &self.right_v,
            right,
            w,
            h,
        )?;

        let left_bg = self.tile_bind_group(&self.left_y, &self.left_u, &self.left_v, "source_left");
        let right_bg =
            self.tile_bind_group(&self.right_y, &self.right_u, &self.right_v, "source_right");

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("source_tiles_encode"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("source_tiles_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Clear so the separator + any letterbox bars read as
                        // the chrome ground colour.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: TILE_CLEAR_COLOR[0],
                            g: TILE_CLEAR_COLOR[1],
                            b: TILE_CLEAR_COLOR[2],
                            a: TILE_CLEAR_COLOR[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &left_bg, &[]);
            pass.set_vertex_buffer(0, self.left_buffer.slice(..));
            pass.draw(0..6, 0..1);
            pass.set_bind_group(0, &right_bg, &[]);
            pass.set_vertex_buffer(0, self.right_buffer.slice(..));
            pass.draw(0..6, 0..1);
        }

        queue.submit(Some(encoder.finish()));
        Ok(())
    }
}

/// Upload one tightly-packed R8 plane into `texture`.
fn upload_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    width: u32,
    height: u32,
) {
    queue.write_texture(
        texture.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_quad_preserves_aspect_in_a_square_tile() {
        // A 2:1 source in a 1:1 tile: width fills, height shrinks.
        let quad = letterbox_quad(-1.0, 0.0, 100.0, 100.0, 200.0, 100.0);
        for v in &quad {
            assert!(
                v.position[0] >= -1.0001 && v.position[0] <= 0.0001,
                "x outside tile half: {:?}",
                v.position
            );
            assert!(
                v.position[1] >= -1.0001 && v.position[1] <= 1.0001,
                "y outside [-1,1]: {:?}",
                v.position
            );
        }
        let uvs: Vec<[f32; 2]> = quad.iter().map(|v| v.uv).collect();
        assert!(uvs.iter().any(|uv| uv == &[0.0, 0.0]));
        assert!(uvs.iter().any(|uv| uv == &[1.0, 1.0]));
    }

    #[test]
    fn letterbox_quad_pillarboxes_a_tall_source() {
        // A 1:2 source in a 2:1 tile: height fills, width shrinks.
        let quad = letterbox_quad(0.0, 1.0, 200.0, 100.0, 100.0, 200.0);
        let ys: Vec<f32> = quad.iter().map(|v| v.position[1]).collect();
        let ymin = ys.iter().cloned().fold(f32::INFINITY, f32::min);
        let ymax = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!((ymin + 1.0).abs() < 1e-4, "ymin {ymin}");
        assert!((ymax - 1.0).abs() < 1e-4, "ymax {ymax}");
        // The horizontal span must be narrower than the tile (pillarbox).
        let xs: Vec<f32> = quad.iter().map(|v| v.position[0]).collect();
        let xmin = xs.iter().cloned().fold(f32::INFINITY, f32::min);
        let xmax = xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(xmin > 0.0, "xmin {xmin} should be inside the tile");
        assert!(xmax < 1.0, "xmax {xmax} should be inside the tile");
    }

    #[test]
    fn letterbox_quad_fills_when_aspects_match() {
        let quad = letterbox_quad(-1.0, 0.0, 100.0, 100.0, 100.0, 100.0);
        let xs: Vec<f32> = quad.iter().map(|v| v.position[0]).collect();
        assert!((xs.iter().cloned().fold(f32::INFINITY, f32::min) + 1.0).abs() < 1e-4);
        assert!((xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max) - 0.0).abs() < 1e-4);
    }

    #[test]
    fn separator_is_the_locked_one_pixel() {
        assert_eq!(SEPARATOR_WIDTH, 1.0);
    }
}
