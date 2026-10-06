<!--
  Lens handles (MANU-05): on-image controls that edit the real `CameraParams`.
  A rim drag drives `k1`, a center-crosshair drag drives `cx`/`cy`, and an
  explicit `Focal length (scale)` mode drives `fx` (with `fy = fx`). `k2..k4`
  live behind an `Advanced lens` disclosure, never on-image.

  Each handle moves exactly one parameter (or named axis). Values are clamped to
  the baseline's travel range before posting (k1 ±0.3, cx/cy ±10% of the frame,
  fx ±15% floored at 5 px); the worker re-clamps and WARNs at the boundary. Every
  handle is a focusable control with an `aria-label` naming the parameter, its
  current value, and its clamp; arrow keys nudge; `Escape` cancels a drag. All
  feedback is the worker's real-parameter warp preview — never a 2-D warp.
-->
<script lang="ts">
  import {
    manual,
    K1_CLAMP,
    CENTER_CLAMP_FRAC,
    FX_CLAMP_FRAC,
  } from "../lib/manual.svelte";
  import type { CameraParamsView, ManualSide } from "../lib/types";

  /** Gain on the rim drag: a full half-frame radial move is +0.3 of k1. */
  const K1_GAIN = 0.6;

  let side = $state<ManualSide>("left");
  let scaleMode = $state(false);
  let advanced = $state(false);
  let wrapEl = $state<HTMLDivElement | null>(null);
  let canvasEl = $state<HTMLCanvasElement | null>(null);

  type Drag = {
    kind: "center" | "rim";
    startRel: { x: number; y: number };
    start: CameraParamsView;
  };
  let drag = $state<Drag | null>(null);

  const current = $derived(
    side === "left" ? manual.params?.left : manual.params?.right,
  );
  const base = $derived(
    side === "left" ? manual.baselineParams?.left : manual.baselineParams?.right,
  );
  const preview = $derived(
    side === "left" ? manual.previewLeft : manual.previewRight,
  );
  const w = $derived(preview?.width || 640);
  const h = $derived(preview?.height || 360);

  const fxSpan = $derived(
    base ? Math.max(Math.max(base.fx, base.fy) * FX_CLAMP_FRAC, 5) : 0,
  );
  const cxSpan = $derived(base ? Math.max(w * CENTER_CLAMP_FRAC, 5) : 0);
  const cySpan = $derived(base ? Math.max(h * CENTER_CLAMP_FRAC, 5) : 0);

  function clamp(v: number, lo: number, hi: number): number {
    return Math.min(hi, Math.max(lo, v));
  }

  /** Clamp against the baseline, then post the edit (MANU-05). */
  function emit(fx: number, cx: number, cy: number, k1: number): void {
    if (!base) return;
    void manual.setLens(side, {
      fx: clamp(fx, base.fx - fxSpan, base.fx + fxSpan),
      cx: clamp(cx, base.cx - cxSpan, base.cx + cxSpan),
      cy: clamp(cy, base.cy - cySpan, base.cy + cySpan),
      k1: clamp(k1, base.k1 - K1_CLAMP, base.k1 + K1_CLAMP),
    });
  }

  /** The pointer position normalized to the frame rectangle `[0,1]`. */
  function rel(e: PointerEvent): { x: number; y: number } | null {
    const el = wrapEl;
    if (!el) return null;
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return null;
    return { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height };
  }

  function startDrag(kind: "center" | "rim", e: PointerEvent): void {
    if (e.button !== 0 || !current) return;
    const r = rel(e);
    if (!r) return;
    // One drag is one undo step (MANU-05).
    manual.snapshotLens(side);
    drag = { kind, startRel: r, start: { ...current } };
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    e.preventDefault();
  }

  function moveDrag(e: PointerEvent): void {
    if (!drag) return;
    const r = rel(e);
    if (!r) return;
    const s = drag.start;
    if (drag.kind === "center") {
      const dx = (r.x - drag.startRel.x) * w;
      const dy = (r.y - drag.startRel.y) * h;
      emit(s.fx, s.cx + dx, s.cy + dy, s.k1);
    } else {
      const r0 = Math.hypot(drag.startRel.x - 0.5, drag.startRel.y - 0.5);
      const r1 = Math.hypot(r.x - 0.5, r.y - 0.5);
      emit(s.fx, s.cx, s.cy, s.k1 + (r1 - r0) * K1_GAIN);
    }
  }

  function endDrag(e: PointerEvent): void {
    drag = null;
    const el = e.currentTarget as HTMLElement;
    if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
  }

  function nudge(kind: "center" | "rim", e: KeyboardEvent): void {
    if (!current) return;
    if (e.key === "Escape" && drag) {
      drag = null;
      e.preventDefault();
      return;
    }
    const step = e.shiftKey ? 10 : 1;
    const k1Step = e.shiftKey ? 0.05 : 0.01;
    let next: [number, number, number, number] | null = null;
    const s = current;
    if (kind === "center") {
      if (e.key === "ArrowLeft") next = [s.fx, s.cx - step, s.cy, s.k1];
      else if (e.key === "ArrowRight") next = [s.fx, s.cx + step, s.cy, s.k1];
      else if (e.key === "ArrowUp") next = [s.fx, s.cx, s.cy - step, s.k1];
      else if (e.key === "ArrowDown") next = [s.fx, s.cx, s.cy + step, s.k1];
    } else {
      if (e.key === "ArrowUp" || e.key === "ArrowRight")
        next = [s.fx, s.cx, s.cy, s.k1 + k1Step];
      else if (e.key === "ArrowDown" || e.key === "ArrowLeft")
        next = [s.fx, s.cx, s.cy, s.k1 - k1Step];
    }
    if (next) {
      manual.snapshotLens(side);
      emit(next[0], next[1], next[2], next[3]);
      e.preventDefault();
    }
  }

  // --- Drawing (readback-canvas idiom, fail closed) --------------------------

  function draw(): void {
    const el = canvasEl;
    if (!el) return;
    const ctx = el.getContext("2d");
    if (!ctx) return;
    if (el.width !== w) el.width = w;
    if (el.height !== h) el.height = h;
    const styles = getComputedStyle(el);
    const dominant =
      styles.getPropertyValue("--color-dominant").trim() || "#1e1e1e";
    const accent = styles.getPropertyValue("--color-accent").trim() || "#4a9eff";
    ctx.fillStyle = dominant;
    ctx.fillRect(0, 0, w, h);
    const p = preview;
    if (p && p.rgba.length === w * h * 4) {
      ctx.putImageData(new ImageData(new Uint8ClampedArray(p.rgba), w, h), 0, 0);
    }
    // The rim ring (the k1 affordance) and the optical-center crosshair.
    ctx.strokeStyle = accent;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.ellipse(w / 2, h / 2, w * 0.42, h * 0.42, 0, 0, Math.PI * 2);
    ctx.stroke();
    if (current) {
      const cxp = (current.cx / w) * w;
      const cyp = (current.cy / h) * h;
      ctx.beginPath();
      ctx.moveTo(cxp - 12, cyp);
      ctx.lineTo(cxp + 12, cyp);
      ctx.moveTo(cxp, cyp - 12);
      ctx.lineTo(cxp, cyp + 12);
      ctx.stroke();
    }
  }

  $effect(() => {
    void preview;
    void current;
    void canvasEl;
    draw();
  });

  const cxPct = $derived(current ? (current.cx / w) * 100 : 50);
  const cyPct = $derived(current ? (current.cy / h) * 100 : 50);

  const centerLabel = $derived(
    current && base
      ? `Optical center, cx ${current.cx.toFixed(0)} cy ${current.cy.toFixed(0)}, clamp ±${cxSpan.toFixed(0)} px`
      : "Optical center",
  );
  const rimLabel = $derived(
    current && base
      ? `Radial distortion (k1) ${current.k1.toFixed(3)}, clamp ±${K1_CLAMP}`
      : "Radial distortion (k1)",
  );
</script>

<section class="lens-handles" aria-label="Lens handles">
  <div class="side-toggle" role="group" aria-label="Camera side">
    <button
      type="button"
      class:active={side === "left"}
      aria-pressed={side === "left"}
      onclick={() => (side = "left")}>Left</button
    >
    <button
      type="button"
      class:active={side === "right"}
      aria-pressed={side === "right"}
      onclick={() => (side = "right")}>Right</button
    >
  </div>

  <div class="handle-canvas-wrap" bind:this={wrapEl}>
    <canvas bind:this={canvasEl} class="handle-canvas" aria-hidden="true"></canvas>
    <button
      type="button"
      class="handle rim"
      aria-label={rimLabel}
      onpointerdown={(e) => startDrag("rim", e)}
      onpointermove={moveDrag}
      onpointerup={endDrag}
      onpointercancel={endDrag}
      onkeydown={(e) => nudge("rim", e)}
    >
      <span class="handle-dot rim-dot" aria-hidden="true"></span>
    </button>
    <button
      type="button"
      class="handle center"
      style="left: {cxPct}%; top: {cyPct}%"
      aria-label={centerLabel}
      onpointerdown={(e) => startDrag("center", e)}
      onpointermove={moveDrag}
      onpointerup={endDrag}
      onpointercancel={endDrag}
      onkeydown={(e) => nudge("center", e)}
    >
      <span class="handle-dot center-dot" aria-hidden="true"></span>
    </button>
  </div>

  <div class="handle-readouts">
    <span class="readout">{rimLabel}</span>
    <span class="readout">{centerLabel}</span>
  </div>

  <button
    type="button"
    class="mode-toggle"
    class:active={scaleMode}
    aria-pressed={scaleMode}
    onclick={() => (scaleMode = !scaleMode)}
  >
    Focal length (scale)
  </button>

  {#if scaleMode && current && base}
    <label class="scale-row">
      <span class="scale-label">Focal length (scale)</span>
      <input
        type="range"
        class="scale-slider"
        min={base.fx - fxSpan}
        max={base.fx + fxSpan}
        step="1"
        value={current.fx}
        aria-label="Focal length (scale), fx {current.fx.toFixed(0)} pixels, clamp ±{fxSpan.toFixed(0)}"
        onfocus={() => manual.snapshotLens(side)}
        oninput={(e) =>
          emit(Number(e.currentTarget.value), current.cx, current.cy, current.k1)}
      />
      <span class="scale-value">{current.fx.toFixed(0)} px</span>
    </label>
  {/if}

  <button
    type="button"
    class="advanced-toggle"
    aria-expanded={advanced}
    onclick={() => (advanced = !advanced)}
  >
    Advanced lens
  </button>
  {#if advanced}
    <p class="advanced-note">
      Higher-order radial terms (k2–k4) stay at the profile value — they are
      edge-only and mutually coupled, so v1 keeps them off the image.
    </p>
  {/if}
</section>

<style>
  .lens-handles {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .side-toggle {
    display: inline-flex;
    gap: var(--space-xs);
  }

  .side-toggle button,
  .mode-toggle,
  .advanced-toggle {
    min-height: 32px;
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .side-toggle button.active,
  .mode-toggle.active {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .side-toggle button:focus-visible,
  .mode-toggle:focus-visible,
  .advanced-toggle:focus-visible,
  .handle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .handle-canvas-wrap {
    position: relative;
    display: block;
    min-width: 0;
    max-width: 100%;
  }

  .handle-canvas {
    display: block;
    width: 100%;
    height: auto;
    aspect-ratio: 16 / 9;
    object-fit: contain;
    background: var(--color-dominant);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
  }

  .handle {
    position: absolute;
    width: 32px;
    height: 32px;
    margin: -16px 0 0 -16px;
    padding: 0;
    border: none;
    background: transparent;
    cursor: grab;
    touch-action: none;
  }

  /* The rim handle sits on the ring at the mid-right edge. */
  .handle.rim {
    left: 92%;
    top: 50%;
  }

  .handle-dot {
    display: block;
    width: 10px;
    height: 10px;
    margin: 11px auto;
    border: 2px solid var(--color-accent);
    border-radius: 50%;
    background: transparent;
  }

  .center-dot {
    border-radius: 0;
    width: 12px;
    height: 12px;
    margin: 10px auto;
  }

  .handle-readouts {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .readout {
    font-family: var(--font-mono);
    font-size: var(--text-label);
    color: var(--color-log-info);
  }

  .scale-row {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
  }

  .scale-label {
    color: var(--color-body-text);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    white-space: nowrap;
  }

  .scale-slider {
    flex: 1;
    accent-color: var(--color-accent);
  }

  .scale-value {
    font-family: var(--font-mono);
    color: var(--color-log-info);
    white-space: nowrap;
  }

  .advanced-note {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
  }
</style>
