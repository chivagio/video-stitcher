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

### A3. The worker/frontend transport contract is implicit, so "ready" is unreachable if any projection is missed

**Impact:** High (PREV-02), and silent. The frontend decides whether the clip is
playable purely by **string-matching the log projection** the worker emits:

- `transport.svelte.ts` starts in `status: "empty"` and gates `play`, `seek`,
  `step`, and `set_loop` on leaving it.
- It leaves only on parsing `"transport: (Playing|Paused|Ended), loop on|off"`
  (or a `position: frame X` line) out of `WorkerEvent::Log`.

So "playback is available" is not a property the worker declares — it is an
emergent consequence of which log lines happen to be emitted on which code path.
Phase 2 hit this immediately: the chrome has no Import button (the app imports at
startup), and the successful-import path emitted no position/transport, so the
store stayed `empty` forever and **Play was unreachable** with no error anywhere.
The defect was invisible to `cargo test` — the Rust unit tests exercise
`WorkerEvent`s directly and never touch the log text the frontend parses — and
only surfaced when the headless probe tried to drive a real session.

**Resolution (Phase 2, gap closure):** the worker now projects the loaded clip's
position + transport from the `Import` command handler, and
`EngineBackend::loaded_transport` was added as a read-only accessor so reporting
state cannot materialize a placeholder transport in the `session` slot
(`session_active()` keys off that slot). Regression test:
`worker::tests::import_projects_position_and_transport_so_the_frontend_can_play`
(verified to fail without the fix). The headless probe
(`scripts/phase2-chrome-probe.sh`) now drives Play end-to-end and reaches
`PHASE2 PROBE: PASS`.

**Residual gap:** the underlying coupling is still there — the frontend derives
readiness from human-readable log text rather than from the typed
`WorkerEvent::Transport { state, .. }` fields it already receives, and the
`parseTransport` regex must be kept in sync with the worker's formatter by hand.
Wiring the rune stores to the typed event payloads is the real fix; it is out of
scope for Phase 2 and is left as the first thing to tighten when the frontend
protocol next changes.

### A4. "0 errors" from `svelte-check` hid a dead UI control — read the warnings

**Impact:** High (PREV-02), and it survived two verification passes. `Timeline.svelte`
computed its slider bounds as plain `const`s:

```js
const max = total !== null && total > 0 ? total - 1 : 0;   // frozen at mount
const value = dragging ? dragFrame : frame;                 // frozen at mount
```

Svelte 5 evaluates the component body once, so both froze at mount — with `total`
null that pinned the range to `min=0 max=0 value=0` permanently. The playhead never
advanced and a mouse drag could never seek, i.e. the `scrub` capability PREV-02 names
was absent. It was masked in the accessibility tree (`aria-valuetext` reads live props)
and keyboard stepping and the Step buttons still worked, so the only honest signal was
`svelte-check`'s **8 warnings** — which both earlier passes reported as "0 errors" and
moved on. `const` -> `$derived` fixed it.

**Resolution (Phase 2, gap closure):** both bindings are `$derived`; `svelte-check` is now
`0 errors and 0 warnings`, and that zero-warning state is the gate, not zero-errors.

**Residual gap:** nothing enforces the gate — `npm run build` succeeds with warnings, and
CI does not run `svelte-check`. Until it does, a non-reactive binding can be reintroduced
silently. The two remaining warning classes at the time of writing were also real defects
(`canvasEl` missing `$state`, and a `tabindex` on a `role="img"` div), which is the
argument for wiring `svelte-check --fail-on-warnings` into CI.

### A5. The engine-side pose path verified clean while pan had zero callers

**Impact:** High (PREV-04 / ROADMAP SC 4), and it passed two verification passes.
The worker, `intent_translator`, and `PoseControl` were all correct and unit-tested for
yaw, pitch, and FOV. What was missing was the frontend caller: `pose.svelte.ts` had a
complete `nudgeYaw` / `nudgePitch` / `nudgeYawStep` / `nudgePitchStep` API with **zero
call sites**, and the preview surface handled only `onwheel`. So "User can pan, zoom, and
adjust FOV" shipped as two of three.

This is the same failure shape as A3 and A4: the half of the feature that is cheap to
test in isolation gets verified, and the seam that actually determines whether a user can
perform the action stays unobserved — here because the only way to observe it is a real
pointer drag, which the headless rig cannot do (`xdotool` synthesises clicks, not drags).

**Resolution (Phase 2, gap closure):** drag-pan is wired on the preview surface with
pointer capture, px→rad is zoom-relative (a full-width drag sweeps one FOV), and
`Shift`+arrow nudges pose (bare arrows stay with the timeline's frame stepping). The step
helpers now take a `-1 | 1` direction — they previously always stepped positive, so left
and right were the same action.

**Residual gap:** the px→rad constant and the grab-the-world sign convention are
frontend-only judgements with no automated coverage; they are called out explicitly in
`02-UAT.md` item 5 so a human confirms the feel rather than inheriting it. The broader
pattern worth watching: a capability is not delivered until its *caller* exists, and
"the engine side is tested" is not evidence that a user can reach it.

