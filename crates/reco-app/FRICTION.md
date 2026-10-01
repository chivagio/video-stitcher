# reco-app Consumer API Friction

API gaps the Tauri host consumer (`reco-app`) hits building the native-surface
presenter against `reco-core`, documented here rather than worked around
(AGENTS.md rule: document friction, don't work around it).

## Active

### A1. `GpuContext::from_device_queue` dropped the adapter (`adapter: None`)

**Impact:** High. The presenter must call `Surface::get_capabilities(&adapter)`
to negotiate surface format/alpha modes, but `GpuContext::from_device_queue`
(`crates/reco-core/src/gpu/mod.rs`) stores `adapter: None`, so the shared-device
path has no adapter to negotiate against. Only `GpuContext::for_surface` retains
`adapter: Some(..)`, and the presenter must NOT call `for_surface` to share —
the presenter takes the device from this flow but does not own it.

**Resolution (this plan, Task 3):** reco-core gained two additive API surfaces
that close the gap without changing any existing caller:

- `GpuContext::adapter(&self) -> Option<&wgpu::Adapter>` — read access to the
  retained adapter (returns `None` only for contexts built by the unmodified
  `from_device_queue`).
- `GpuContext::from_device_queue_with_adapter(device, queue, adapter_info,
  adapter)` — a sibling constructor that carries the adapter, so a consumer
  that owns an adapter + device can build a context that still negotiates
  surfaces. `from_device_queue` keeps its `adapter: None` semantics verbatim.

**Residual gap:** none observed. The `reco-app` device-creation path uses
`for_surface` (which retains the adapter), then `gpu.adapter()` feeds
`SurfacePresenter::configure`. `grep -c request_adapter crates/reco-app/src/`
is 0 — the app never requests a second adapter. A future consumer that only has
a raw `Device`/`Queue` (no adapter handle) would still need to retain an adapter
itself; the new constructor makes that explicit rather than impossible.

The `GpuContext` adapter field's `#[cfg_attr(not(target_os = "windows"),
allow(dead_code))]` attribute is now stale-ish (the field is read cross-platform
via `adapter()`), but left in place to keep the diff additive; a follow-up
cleanup can drop it.

### A2. No two-tile raw-source render path in reco-core

**Impact:** Medium (PREV-03). The source↔panorama comparison toggle needs to
draw the left and right *raw* decoded frames tiled side-by-side into the
presenter's acquired surface view (or the readback target). No `reco-core` API
does this:

- `LensPreviewRenderer` (`crates/reco-core/src/lens/preview.rs`) renders **one**
  camera through the fisheye shader and **allocates and returns its own
  `wgpu::Texture`** each frame; it cannot draw into a caller-provided view, so a
  consumer would have to build a second composition pass (and own the texture)
  outside the engine.
- `StitchPipeline`/`StitchRenderer` only draw the **stitched panorama**; there is
  no path that uploads the two sources and paints them as independent tiles.
- The nearest analogue (`LensPreviewRenderer`) also wants a whole `GpuContext`
  rather than the shared device/queue the presenter already holds (D-03).

Working around this in `reco-app` would mean re-implementing YUV upload + a
shader + a pipeline in the consumer crate — exactly the kind of engine-API
workaround the project rule forbids.

**Resolution (Phase 2, plan 02-04):** `reco-core` gained an additive render path
that closes the gap without changing any existing caller:

- `render::source_tiles::SourceTileRenderer` — a renderer that uploads the two
  YUV420P sources and draws them into a **caller-provided** `wgpu::TextureView`,
  creating its pipeline/textures on the **caller-supplied device** (the shared
  worker device). Left/right tiles use contain letterboxing (aspect preserved,
  never stretched) with a 1px separator between the halves.
- `StitchRenderer::render_source_tiles(left, right, view)` — builds and caches an
  internal `SourceTileRenderer` lazily (rebuilt only on a format/viewport change)
  and draws into the given view. No readback, no second device, no per-toggle
  allocation.

**Residual gap:** none observed. A future consumer that wants raw tiles with a
different layout (e.g. stacked vertically, or with per-tile overlays) would still
need its own draw path; the new renderer is intentionally fixed to the UI-SPEC's
"left | right side-by-side" layout.

