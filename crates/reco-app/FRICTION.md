# reco-app Consumer API Friction

API gaps the Tauri host consumer (`reco-app`) hits building the native-surface
presenter against `reco-core`, documented here rather than worked around
(AGENTS.md rule: document friction, don't work around it).

## Active

### A1. `GpuContext::from_device_queue` drops the adapter (`adapter: None`)

**Impact:** High. The presenter must call `Surface::get_capabilities(&adapter)`
to negotiate surface format/alpha modes, but `GpuContext::from_device_queue`
(`crates/reco-core/src/gpu/mod.rs`) stores `adapter: None`, so the shared-device
path has no adapter to negotiate against. Only `GpuContext::for_surface` retains
`adapter: Some(..)`, and the presenter must NOT call `for_surface` — that would
mint a second device, violating D-03/FOUND-03.

**Current posture:** documented, not worked around. The worker retains the
`wgpu::Adapter` from the adapter-retaining device-creation path and passes a
reference to the presenter for capability negotiation. See
`crates/reco-app/src/presenter/mod.rs` (`SurfacePresenter::configure` takes an
`&Adapter`).

**Requested engine change:** an additive `GpuContext` accessor (e.g.
`fn adapter(&self) -> Option<&wgpu::Adapter>`) so the presenter does not need the
worker to thread the adapter separately. Tracked for Task 3 of this plan.
