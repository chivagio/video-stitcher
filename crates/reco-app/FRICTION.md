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

### A6. Pose sign conventions are undocumented, and two doc comments disagree

**Impact:** Medium (PREV-04), invisible to every gate. `PoseControl` treats **+yaw as
looking LEFT** and **+pitch as looking UP**. Nothing in `reco-control` or the GUI states
this, and the two comments written while wiring pan (the drag handler and the arrow-key
handler) asserted opposite conventions. The result shipped as two spellings of the same
gesture pointing opposite ways: drag right looked right, `Shift`+`Right` looked left.

The only dependable references are `crates/reco-cli/src/preview.rs:582-598` (the CLI's
arrow mapping, which is correct) and the `view_matrix` itself. **Neither doc comment can
be trusted here.**

**Resolution (Phase 2, gap closure):** both paths now follow the CLI mapping —
`Left = +yaw`, `Right = -yaw`, `Up = +pitch`, `Down = -pitch` — and the `nudgeYawStep`
doc now states the sign convention explicitly instead of naming the screen direction.
`YAW_DRAG_SIGN` / `PITCH_DRAG_SIGN` remain named constants so the drag convention is
flippable in one place, and `02-UAT.md` item 5 cross-checks drag against arrows so a human
confirms the feel rather than trusting either comment.

**Residual gap:** the convention is stated only in comments and in the CLI. A flipped
constant passes `svelte-check`, the frontend build, and the probe — there is no frontend
test runner in `package.json`, and no automated signal exists for this class of bug. Two
options worth taking later: a tiny test that asserts the four arrow signs against the CLI
mapping, and a `reco-control` doc line on `ViewportPosition` stating that +yaw looks left.

As of plan 02-10 the drag convention has **two** implementations — this frontend path and the
Rust native path in `crates/reco-app/src/presenter/pointer_input.rs` (`YAW_DRAG_SIGN` /
`PITCH_DRAG_SIGN`, pinned by unit tests against the CLI) — and they must be flipped together,
or the pan direction will differ between the native and readback presenters.

### A7. A `#[tauri::command]` with a snake_case argument silently never fires

**Impact:** High (PREV-01 / PREV-02 chrome), and it was the real cause of the UAT gap
recorded as "the native child window is never resized". Tauri resolves each command
argument by **one** payload key — `InvokeBody::Json(v) => v.get(self.key)` in
`tauri/src/ipc/command.rs` — and `#[tauri::command]` defaults that key to **camelCase**.
`commands::set_chrome` took `panel_expanded` / `drawer_expanded`, so the generated key was
`panelExpanded`, while `App.svelte` sent `{ panel_expanded, drawer_expanded }` to match the
rest of the typed protocol (which is snake_case everywhere: `WorkerCommand::SetChrome`,
`ViewMode`/`PresenterKind`'s `serde(rename_all = "snake_case")`, every other payload).

Deserialization therefore failed with `command set_chrome missing required key
panelExpanded` and the handler never ran. The frontend does `void invoke("set_chrome", …)`,
which swallows the rejection, so there was **no error anywhere**: the controls panel visibly
expanded in the webview while the panorama kept painting over it, `xwininfo` kept reporting
the child as 1240x728, and every Rust unit test stayed green because none of them cross the
IPC boundary. `set_view` was unaffected only because `mode` is a single word — the only other
multi-word argument in the app (`preview_attach_readback`'s `on_frame`) happened to be called
camelCase from JS, so the crate was inconsistent *and* broken at the same time.

**Resolution (Phase 2, gap closure):** `commands::set_chrome` now carries
`#[tauri::command(rename_all = "snake_case")]`, which pins the IPC key names to the crate's
established snake_case protocol. Verified by driving the real app: the panel-toggle click now
produces `viewport reconfigured to 1000x728` in the worker log and
`xwininfo -id <child>` reports the child at 1000x728, where before the fix no `set_chrome`
line appeared at all. `scripts/phase2-chrome-probe.sh` asserts that log line, so the seam is
now covered end-to-end.

**Residual gap:** nothing type-checks the JS payload keys against the Rust parameter names —
`crates/reco-app/ui` has no test runner, and `svelte-check` cannot see it. The same class of
breakage will reappear the next time a multi-word argument is added. The durable fix is one
`rename_all = "snake_case"` at the app level (e.g. on the `Builder`) so the whole IPC surface
is snake_case by construction and the crate stops carrying two spellings; the residual risk
until then is only for newly added multi-word arguments.

### A8. A session constructor that silently drops user-set state

**Impact:** High (PREV-02), and silent. `EngineBackend::transport()` prefers the live
session and falls back to the import-built transport, so a `SetLoop` issued with no session
active lands on `loaded` — but `begin_preview` builds a brand-new `Transport`, and
`Transport::new` hard-codes `loop_enabled: false`. The flag was stored correctly and then
thrown away at the session boundary, with no error anywhere: the user sees the clip stop at
its end instead of wrapping. The generalisable name for this is **a constructor that silently
drops user-set state** — the setting round-trips through every layer that inspects it, so the
only place it can die is the line that builds the next one.

**Resolution (Phase 2, plan 02-09):** `transport::carry_user_state(previous, next)` is the
single named seam for state that must survive a session boundary, and
`worker::new_session_transport` is the shared constructor that applies it — to the real backend
*and* to the GPU-free mock, so the mock cannot drift from the real behaviour.
`GpuEngineBackend::begin_preview` carries before any projection is emitted; `end_preview`
mirrors a session's flag back into `loaded`, so a flag set during a session also survives that
session ending (a failed tick ends it too). Five tests cover it; one is mutation-proven.

**Residual gap:** `Transport` still has no general notion of "user-set vs derived" state —
`carry_user_state` carries one field by name, so the next user-settable field can still be
forgotten. A durable fix is a `Transport::inheriting_from(&self, next: Self)` constructor that
makes the carry unskippable by construction; not taken here because it changes
`Transport::new`'s shape for every caller. The mutation proof also has a blind spot worth
remembering: while the two backends built their session transport on separate lines, deleting
the carry from the *real* backend left every mock-driven test green. Sharing the constructor
closed that hole; it does not close the general one, where a GPU-free mock cannot observe a
GPU-backed path at all.

### A9. A mock backend that mirrors the real one only by convention

**Impact:** Medium, and it masked a High defect for one plan. `MockBackend` is what makes the
worker loop, its ordering and its command protocol testable without a GPU — but nothing tied
it to `GpuEngineBackend`. When plan 02-09 added the session-boundary carry, the tests passed
against the mock's own copy of the behaviour while the real backend's call site was a separate
line that could be deleted with every gate still green. Same shape as A3, A4, A5 and A7: the
half that is cheap to test in isolation gets verified, and the seam that actually determines
what a user gets stays unobserved.

**Resolution (Phase 2, plan 02-09):** the session transport is built by one free function,
`worker::new_session_transport`, called from both backends, and its precedence (session, then
`loaded`) is asserted directly by
`worker::tests::new_session_transport_carries_from_the_session_then_the_loaded_transport`
without a GPU. The mutation proof now bites: removing the carry from that one function fails
three tests, where previously it failed none.

**Residual gap:** only the seams that were factored out are protected. The mock still
duplicates `tick_session`'s shape, `end_preview`'s bookkeeping and the presenter's error
handling by hand, so the next change to any of those can drift the same way. Nothing — compile
time or CI — keeps a `MockBackend` in step with a `GpuEngineBackend`; the honest cheap signal
remains the headless probe (`scripts/phase2-chrome-probe.sh`), which drives the real binary.

### A10. A child window takes the input a webview drawn into its parent can never have

**Impact:** High (PREV-04), and it shipped as a feature that did nothing. The
panorama is an X11 **child window** of the main window and the WebKitGTK webview is
**not** a separate X window — GTK draws it into the parent's own surface. An X child
composites above its parent's own drawing and receives every pointer event in its
area, so the webview's `onpointerdown` / `onpointermove` / `onwheel` over the preview
region could never fire: a 500 px drag and five wheel notches during live playback
changed zero pixels, and the frontend's path was green in `svelte-check`, green in
the build, and unreachable.

Two consequences follow, and both are architecture rather than bug:

1. **A native presenter and a webview presenter cannot share one pose-input route.**
   Each needs its own, and which one is live is a property of the *presenter*, not of
   the view. The webview route is now gated on `presenterKind !== "native"`; in native
   mode it is inert by construction, and in readback / separate-window mode it is the
   only route there is (swapping presenters destroys the outgoing child window, so
   nothing native is left over the region).
2. **The child owns the input.** `X11Presenter` selects the button/motion/wheel masks
   and drains them with `XCheckWindowEvent`, and the worker translates the gesture
   through `presenter::pointer_input::pointer_gesture_to_intents` — the same
   `dispatch_intent` the typed `WorkerCommand::Intent` path uses.

The same shared-`Display*` hazard that makes this necessary is the one that makes it
dangerous: GTK and this presenter hold the *same* `Display*`, so `XPending` +
`XNextEvent` would drain GTK's own queue and silently break the entire UI. The drain
is window-and-mask scoped for that reason, and there is a negative grep
(`XNextEvent`/`XPending(` must not appear) plus a mutation proof guarding it.

**Resolution (Phase 2, plan 02-10):** see above. The translation itself is pure —
no X types, no GPU — so its sign convention is pinned by unit tests that run in CI
where there is no X server at all.

**Residual gap:** the drag sign convention now has **two** implementations, this
frontend path and `presenter::pointer_input`'s `YAW_DRAG_SIGN` / `PITCH_DRAG_SIGN`
(see A6), and nothing keeps them in step: `crates/reco-app/ui` has no test runner, so
a flip on one side is invisible to every automated gate. The honest cheap signal is
the live drag assertion in `scripts/phase2-chrome-probe.sh`. Unrelatedly, the
unified drag/wheel path in `PoseControl` (`drag_deg_per_pixel`, `wheel_fov_per_tick`,
`invert_drag_x`/`y`) is now unused by this consumer — it is a second place the same
conversion is configured, and the two will drift.

## A10 — a fire-and-forget event bridge silently drops the opening projection

**Symptom.** After a host reboot, the app rendered correctly but Play was
greyed out with no error anywhere, the clip length read `0:00 / 0:00`, and the
panel showed default pose values. The import had plainly succeeded (the log
shows `import finished` and both NVDEC decoders opening).

**Cause.** `install_event_bridge` does `Emitter::emit(&app, "worker-event", …)`
per event, and Tauri's `emit` is a fan-out to the listeners registered *at that
moment*. The worker is spawned in `setup()` and imports immediately, so its
first position/transport/pose/view events fire roughly a second into startup —
before the webview has loaded its bundle and run `listen`. They are dropped with
no error, because there is no listener to receive them and no error to report.

The frontend then sits in its initial `status: "empty"`, which gates every
transport action. Nothing in the UI says "waiting for the worker"; it just looks
disabled.

**Why it hid for so long.** Every other Phase 2 symptom was verified through
*later* commands (panel toggle, view switch, Play once reachable), and those all
emit after the listener is registered. So the only thing being dropped was the
one projection that happens before boot completes. The same race also silently
breaks a webview reload: every projection since boot is missed, so the UI comes
back empty with no reload-time recovery.

**Fix.** `republish_projection` — the frontend subscribes, *then* asks the
worker to re-assert the whole projection. Two consequences worth keeping:

- Ordering is load-bearing. The reconcile must land after `listen` resolves, or
  it is dropped the same way. This is why `App.svelte` now awaits the four
  `init()` calls sequentially in one async task: the previous four independent
  `void` calls had no defined order relative to the invoke.
- It doubles as reload recovery, which is why it is a worker command and not a
  frontend-side default. The frontend still never derives readiness itself.

**Consumer lesson.** A projection the worker emits only once at startup is
effectively a handshake with a listener that may not exist yet. Anything emitted
before the frontend subscribes needs a pull-based counterpart. Where a store
`init()` is fire-and-forget, the ordering it implies should be explicit.

**Test-coverage gap (open).** The three tests drive `MockBackend`. The real
`GpuEngineBackend::republish_projection` — the one that actually runs — is only
covered by the live observation (Play glyph 137 -> 232, timecode `0:00 / 0:00` ->
`0:00 / 0:02`). A mock-only test of a projection path cannot catch a wrong field
on the real backend; see A5 for the same shape. Worth an assertion on the real
backend's field reads once a GPU-free seam exists for it.

## A11 — a mock that reimplements a decision hides the defect at the real site

**Symptom.** Three separate defects reached a manual UAT run with a fully green
suite: the wheel did nothing, Play did nothing after the clip ended, and Loop
froze the UI. All three are in the end-of-source / input paths.

**The wheel one is the generalisable lesson.** A wheel notch is a `ButtonPress`
whose *detail* is 4/5 (X.h `Button4`/`Button5`) — its event type is
`ButtonPress`, like any click. The press arm matched only `button == 1`, and a
separate detail-based arm sat further down the `kind` match, unreachable because
no wheel event can have any type other than `ButtonPress`. Every scroll was
consumed and discarded, and dragging panned correctly the whole time, so the
obvious "is input wired up at all" check passed.

It survived review because the mapping lived inside an `unsafe` block next to a
live `Display*`, where it reads as plumbing. Extracting `PointerState::apply_event`
as a pure function of `(kind, button, x, y)` made it testable, and three of the
six new tests fail when the original one-line bug is reintroduced.

**The structural cause of the other two.** `MockBackend::tick_session`
reimplemented the wrap-vs-end branch instead of sharing it. Mutating the real
site — deferring the rewind to a pending seek, the actual defect — left every
test green, because the mock never ran that code.

That reimplementation was itself introduced by an earlier fix: making the mock
carry the Loop flag across the session boundary meant teaching it the loop path
too. A mock that gains behaviour tends to grow its own copy of the decision.

**Consumer lesson.** A test double should call the same function the real
implementation calls. When a mock has to *model* something (a source that
exhausts, a window that steals input), model the thing — not the branch that
reacts to it. `resolve_end_of_source` is now a provided trait method with one
body, run by both backends.

**Corollary.** Where a real cost is invisible to the double (a decode-pipeline
respawn opening a CUDA context), assert on the observable proxy — the rewind
count against the tick count — not on the thing you cannot see. `ticks >
rewinds` is the invariant; a specific ratio would have over-fitted the clip
length and was wrong on the first run.

## A12 — typed worker events never reach stdout, so nothing headless can observe them

**Symptom.** Plan 02-11 specified its assertion chain as ending in
`INFO 'pose: yaw …'`. The first run of that assertion exited 1 with **no FAIL
line at all** — the worst outcome a gate can produce.

**Two causes, one of which is a project-wide trap.**

`EventSink` has two kinds of method. `info`/`failed` go through `log()`, which
mirrors to `log::info!` and therefore to the terminal, CI logs, and any headless
probe. `pose`, `position`, and `transport` send a typed `WorkerEvent` straight
onto the channel — only the webview ever sees them.

So there is a class of state that is **invisible to every automated check**,
silently, while looking perfectly well-connected: `events.pose(...)` compiles,
tests asserting the event can be written against the mock, and the UI shows the
right numbers. A probe reading stdout just never sees it. The FRICTION notes had
recorded this fact earlier (Pose/Position/Transport reach only the webview) but
no plan accounted for it when choosing an assertion target.

**Consumer lesson.** Before writing an assertion against the worker's log,
check *which* kind of method emits it. `EventSink::log` reaches stdout;
everything else needs a different observation channel (screenshot, in-app drawer)
or an explicit new line.

**Second cause — `set -euo pipefail` plus a pipeline that can find nothing.**
The reading helper ended in `grep`, which returns 1 on no match. That failed the
*command substitution in the assignment itself*, so the script terminated before
the caller's `-z` emptiness check could run. Every helper that returns a value a
caller inspects must exit 0 and signal "nothing" by returning empty. An empty
string a caller can report beats a process that dies without explanation — a
silent death hides the very failure the gate exists to catch.

**Related:** the FOV slider advertised 40-150° while `clamp_via_coverage` pinned
the pose to 50.87° for the shipped clip. The CLI logged `max FOV =
… (coverage-limited)`; the app surfaced nothing. Bound the control to the value
the engine reports rather than to a constant chosen by the UI — a range the
engine will never accept is indistinguishable from a broken control.
