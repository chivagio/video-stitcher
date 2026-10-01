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

