<!--
  Preview surface (UI-SPEC Component Inventory).
  Mode-aware host: transparent (native) · placeholder (separate window) ·
  readback canvas (degraded).
-->
<script lang="ts">
  import { Channel } from "@tauri-apps/api/core";
  import { pose } from "../lib/pose.svelte";
  import StateOverlay from "./StateOverlay.svelte";
  import type { PresenterKind } from "../lib/types";

  let {
    presenterKind,
    viewMode,
    onAttachReadback,
    onShowPreviewWindow,
  }: {
    presenterKind: PresenterKind;
    viewMode: "source" | "panorama";
    onAttachReadback: (channel: Channel<ArrayBuffer>) => void;
    onShowPreviewWindow: () => void;
  } = $props();

  const isNative = $derived(presenterKind === "native");
  // Pose input mapping (UI-SPEC "panorama mode only"), plus the window-ownership
  // rule. Two gates, in this order:
  //
  //   1. VIEW: pan and zoom apply to the panorama only. In source view the
  //      region shows the two raw tiles, which `render_source` draws without
  //      the pose, so a pan there would be silently invisible.
  //   2. PRESENTER: only the presenters that draw NO native window over this
  //      region can have the webview own their pointer input. The native
  //      presenter's panorama is an X11 CHILD window of the main window and the
  //      WebKitGTK webview is not a separate X window (GTK draws it into the
  //      parent's own surface), so the child composites above the parent's own
  //      drawing and takes every pointer event in its area: the webview cannot
  //      see those events at all. The Rust side owns them instead
  //      (`X11Presenter::take_pointer_gesture`). Swapping presenters calls
  //      `release_presenter_window()` on the outgoing one, which destroys that
  //      child window, so in readback / separate-window mode nothing native
  //      covers this region and these handlers are the ONLY route to the pose.
  //
  // So this is not "native has a second path": in native mode the region is a
  // hole the webview cannot see, and these handlers are inert there by
  // construction.
  const webviewPoseActive = $derived(viewMode === "panorama" && presenterKind !== "native");
  const isSeparateWindow = $derived(presenterKind === "separate_window");
  const isReadback = $derived(presenterKind === "readback");

  let canvasEl = $state<HTMLCanvasElement | null>(null);
  let surfaceEl = $state<HTMLDivElement | null>(null);

  // --- Drag to pan (CONTEXT D-03: "mouse drag = pan (yaw/pitch)") -------------
  //
  // The px -> rad conversion is zoom-relative: dragging the full width of the
  // preview sweeps one horizontal FOV, so panning feels the same at 40 deg and
  // at 150 deg. The pose store stays protocol-typed (radians) and does no
  // layout math — the viewport width is only known here.
  //
  // Signs come from the ENGINE convention, not from screen intuition:
  //   +yaw looks LEFT  (so drag right -> negative yaw -> camera turns right)
  //   +pitch looks UP  (so drag down  -> negative pitch -> camera looks down)
  // That is the CLI's arrow mapping (`crates/reco-cli/src/preview.rs:582-598`:
  // Left = +yaw, Right = -yaw, Up = +pitch, Down = -pitch), so a drag and the
  // Shift+arrow nudge agree on BOTH axes.
  //
  // They stay named constants so the convention is one obvious place to flip --
  // and the Rust native path carries the identical pair in
  // `crates/reco-app/src/presenter/pointer_input.rs` (`YAW_DRAG_SIGN` /
  // `PITCH_DRAG_SIGN`, pinned by unit tests against the CLI). THE TWO MUST BE
  // CHANGED TOGETHER, or the pan direction will differ between the native and
  // readback presenters.
  const YAW_DRAG_SIGN = -1;
  const PITCH_DRAG_SIGN = -1;

  let panning = $state(false);
  let lastX = 0;
  let lastY = 0;

  function radPerPixel(): number {
    const width = surfaceEl?.clientWidth ?? 0;
    if (width <= 0) return 0;
    const fovRad = (pose.fovValue * Math.PI) / 180;
    return fovRad / width;
  }

  function handlePointerDown(e: PointerEvent): void {
    if (!webviewPoseActive || e.button !== 0) return;
    panning = true;
    lastX = e.clientX;
    lastY = e.clientY;
    // Keep receiving moves if the cursor leaves the region mid-drag.
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }

  function handlePointerMove(e: PointerEvent): void {
    if (!panning) return;
    const k = radPerPixel();
    if (k === 0) return;
    const dx = e.clientX - lastX;
    const dy = e.clientY - lastY;
    lastX = e.clientX;
    lastY = e.clientY;
    // Deltas are additive on the worker, so one intent per axis per move is
    // correct; the pose eases toward the accumulated target each tick.
    void pose.nudgeYaw(YAW_DRAG_SIGN * dx * k);
    void pose.nudgePitch(PITCH_DRAG_SIGN * dy * k);
  }

  function endPan(e: PointerEvent): void {
    if (!panning) return;
    panning = false;
    const el = e.currentTarget as HTMLElement;
    if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
  }

  function paintFrame(buffer: ArrayBuffer): void {
    if (!canvasEl) return;
    const ctx = canvasEl.getContext("2d");
    if (!ctx) return;
    const w = canvasEl.width;
    const h = canvasEl.height;
    const imageData = new ImageData(new Uint8ClampedArray(buffer), w, h);
    ctx.putImageData(imageData, 0, 0);
  }

  // `$effect` keyed on `isReadback`, NOT `onMount`. The session starts on the
  // native presenter, so `onMount` saw `isReadback === false` and bailed — and
  // because the presenter is swappable at runtime, the readback arm would have
  // rendered its canvas with no Channel ever created, leaving a blank frame
  // under the degraded banner. Re-running on the transition attaches the Channel
  // exactly when the readback arm becomes active.
  //
  // Cleanup only clears the JS handler: `preview_attach_readback` takes a
  // non-nullable channel, so there is no detach to send. A later attach replaces
  // the stored channel anyway.
  $effect(() => {
    if (!isReadback) return;
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (buffer: ArrayBuffer) => {
      paintFrame(buffer);
    };
    onAttachReadback(channel);
    return () => {
      // A no-op rather than `undefined`: Tauri's Channel types the handler as
      // required, and detaching is what we mean.
      channel.onmessage = () => {};
    };
  });
</script>

<div
  class="preview-surface"
  class:native={isNative}
  class:separate-window={isSeparateWindow}
  class:readback={isReadback}
  class:pose-active={webviewPoseActive}
  role="img"
  aria-label={viewMode === "source" ? "Source" : "Panorama"}
  class:panning
  bind:this={surfaceEl}
  onpointerdown={handlePointerDown}
  onpointermove={handlePointerMove}
  onpointerup={endPan}
  onpointercancel={endPan}
  onwheel={(e) => {
    if (webviewPoseActive) {
      e.preventDefault();
      const delta = e.deltaY > 0 ? -1 : 1;
      void pose.nudgeFov(delta);
    }
  }}
>
  {#if isNative}
    <!-- Native mode: the webview leaves the region transparent and unpainted.
         The native panorama renders here. No background, border, or shadow. -->
  {:else if isSeparateWindow}
    <StateOverlay
      state={{
        kind: "empty",
      }}
    />
    <div class="placeholder-content">
      <h2 class="placeholder-heading">Preview is in a separate window</h2>
      <p class="placeholder-body">
        Use "Show preview window" to bring the panorama forward.
      </p>
      <button
        type="button"
        class="placeholder-btn"
        onclick={onShowPreviewWindow}
      >
        Show preview window
      </button>
    </div>
  {:else if isReadback}
    <div class="readback-banner">
      <p class="readback-text">
        Degraded preview (readback) — Frames are copied to the UI and throttled.
        For smooth playback use a separate preview window.
      </p>
    </div>
    <canvas
      bind:this={canvasEl}
      class="readback-canvas"
      width={1240}
      height={728}
    ></canvas>
  {/if}
</div>

<style>
  .preview-surface {
    position: fixed;
    left: 0;
    top: 0;
    width: calc(100% - var(--controls-panel-collapsed-width));
    height: calc(100% - var(--transport-bar-height));
    background: transparent;
    border: 0;
    box-shadow: none;
    overflow: hidden;
  }

  .preview-surface.native {
    /* Native mode: fully transparent — the native panorama shows through. */
    background: transparent;
  }

  .preview-surface.separate-window {
    background: var(--color-dominant);
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
  }

  /* A drag pans, so the region must not also scroll/zoom the page, and the
     cursor should say so. Keyed on `webviewPoseActive` (panorama view AND a
     presenter whose pointer input the webview owns), matching the gate the
     handlers use -- in native mode the cursor here is the panorama's, not the
     webview's, so a grab cursor would be a lie. */
  .preview-surface.pose-active {
    touch-action: none;
    cursor: grab;
  }

  .preview-surface.pose-active.panning {
    cursor: grabbing;
  }

  .preview-surface.readback {
    background: var(--color-dominant);
    display: flex;
    flex-direction: column;
  }

  .placeholder-content {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-md);
    padding: var(--space-lg);
    text-align: center;
  }

  .placeholder-heading {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .placeholder-body {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    line-height: var(--line-body);
    color: var(--color-log-info);
    max-width: 400px;
  }

  .placeholder-btn {
    padding: var(--space-sm) var(--space-md);
    border: 1px solid var(--color-accent);
    border-radius: var(--space-xs);
    background: var(--color-accent);
    color: var(--color-dominant);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .placeholder-btn:hover {
    filter: brightness(1.1);
  }

  .readback-banner {
    padding: var(--space-sm) var(--space-md);
    background: var(--color-log-warn);
    color: var(--color-dominant);
  }

  .readback-text {
    margin: 0;
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
  }

  .readback-canvas {
    flex: 1;
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
</style>
