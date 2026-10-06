<!--
  Pin canvas (MANU-03): the direct-manipulation surface for correspondence pins.
  Paints the left/right reference previews side by side and overlays the pin
  markers (verified = success, unverified = warn, active = accent), plus the
  in-progress left point of a half-entered pair. Pointer capture drives drag;
  pin handles are real keyboard-focusable buttons with arrow-key nudging and a
  Delete path; Escape cancels an in-progress pairing.

  The canvas fails closed on a malformed preview payload — a zero dimension or a
  length that does not match `width * height * 4` — so a bad event cannot throw
  inside a pointer handler (the PreviewSurface / MatchOverlay idiom). The
  webview owns this surface; it never composites over the Rust-owned native view.
-->
<script lang="ts">
  import { manual, type ManualPreview } from "../lib/manual.svelte";

  let {
    activeId = $bindable<number | null>(null),
    pairing = $bindable(false),
    pendingLeft = $bindable<[number, number] | null>(null),
  }: {
    activeId?: number | null;
    pairing?: boolean;
    pendingLeft?: [number, number] | null;
  } = $props();

  let canvasEl = $state<HTMLCanvasElement | null>(null);

  /** Gap in intrinsic canvas pixels between the two frames. */
  const GAP = 4;
  /** Fallback frame size when no preview has landed yet. */
  const FALLBACK_W = 640;
  const FALLBACK_H = 360;
  /** Pin hit radius in intrinsic pixels (the 32px target is the button). */
  const HIT_PX = 14;

  /** An active pin drag (optimistic; the worker is told on release). */
  type Drag = { id: number; side: "left" | "right"; px: [number, number] };
  let drag = $state<Drag | null>(null);

  /** The frame geometry in intrinsic canvas pixels. */
  const geom = $derived.by(() => {
    const lw = manual.previewLeft?.width || FALLBACK_W;
    const lh = manual.previewLeft?.height || FALLBACK_H;
    const rw = manual.previewRight?.width || FALLBACK_W;
    const rh = manual.previewRight?.height || FALLBACK_H;
    const leftX = 0;
    const rightX = lw + GAP;
    return {
      lw,
      lh,
      rw,
      rh,
      leftX,
      rightX,
      totalW: lw + rw + GAP,
      totalH: Math.max(lh, rh),
    };
  });

  /** The pins with the optimistic drag position merged in. */
  const displayPins = $derived(
    manual.pins.map((p) => {
      if (drag && p.id === drag.id) {
        return drag.side === "left"
          ? { ...p, left_px: drag.px }
          : { ...p, right_px: drag.px };
      }
      return p;
    }),
  );

  // --- Geometry helpers -------------------------------------------------------

  function pointerPx(e: PointerEvent): { x: number; y: number } | null {
    const el = canvasEl;
    if (!el) return null;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return null;
    return {
      x: (e.clientX - rect.left) * (geom.totalW / rect.width),
      y: (e.clientY - rect.top) * (geom.totalH / rect.height),
    };
  }

  function sideFor(x: number): "left" | "right" {
    return x < geom.lw + GAP / 2 ? "left" : "right";
  }

  function toFramePx(
    p: { x: number; y: number },
    side: "left" | "right",
  ): [number, number] {
    return side === "left" ? [p.x - geom.leftX, p.y] : [p.x - geom.rightX, p.y];
  }

  /** The pin (id + side) under `p`, or null. */
  function hitPin(p: { x: number; y: number }): {
    id: number;
    side: "left" | "right";
  } | null {
    let best: { id: number; side: "left" | "right" } | null = null;
    let bestDist = HIT_PX;
    for (const pin of displayPins) {
      const candidates: Array<{ side: "left" | "right"; x: number; y: number }> =
        [
          { side: "left", x: geom.leftX + pin.left_px[0], y: pin.left_px[1] },
          {
            side: "right",
            x: geom.rightX + pin.right_px[0],
            y: pin.right_px[1],
          },
        ];
      for (const c of candidates) {
        const d = Math.hypot(c.x - p.x, c.y - p.y);
        if (d <= bestDist) {
          bestDist = d;
          best = { id: pin.id, side: c.side };
        }
      }
    }
    return best;
  }

  // --- Pointer interactions ---------------------------------------------------

  function onCanvasPointerDown(e: PointerEvent): void {
    if (e.button !== 0) return;
    const p = pointerPx(e);
    if (!p) return;

    const hit = hitPin(p);
    if (hit) {
      activeId = hit.id;
      drag = { id: hit.id, side: hit.side, px: toFramePx(p, hit.side) };
      canvasEl?.setPointerCapture(e.pointerId);
      e.preventDefault();
      return;
    }

    const side = sideFor(p.x);
    if (side === "right") {
      // A right-frame click only completes an in-progress pair.
      if (pendingLeft !== null) {
        const left = pendingLeft;
        const rightPx = toFramePx(p, "right");
        pendingLeft = null;
        pairing = false;
        void manual.addPin(left, rightPx);
      }
      return;
    }
    // Left-frame click: start (or restart) the pair.
    pendingLeft = toFramePx(p, "left");
    pairing = true;
  }

  function onCanvasPointerMove(e: PointerEvent): void {
    if (drag === null) return;
    const p = pointerPx(e);
    if (!p) return;
    drag = { ...drag, px: toFramePx(p, drag.side) };
  }

  function onCanvasPointerUp(e: PointerEvent): void {
    if (drag !== null) {
      const { id, side, px } = drag;
      drag = null;
      void manual.movePin(id, side, px);
    }
    if (canvasEl?.hasPointerCapture(e.pointerId)) {
      canvasEl.releasePointerCapture(e.pointerId);
    }
  }

  function onCanvasKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape" && pairing) {
      pendingLeft = null;
      pairing = false;
      e.preventDefault();
    }
  }

  // --- Pin handle (keyboard-focusable) interactions ---------------------------

  function onHandlePointerDown(
    e: PointerEvent,
    id: number,
    side: "left" | "right",
  ): void {
    e.stopPropagation();
    if (e.button !== 0) return;
    activeId = id;
    const p = pointerPx(e);
    const px = p ? toFramePx(p, side) : null;
    if (px) drag = { id, side, px };
    canvasEl?.setPointerCapture(e.pointerId);
  }

  function onHandleKeyDown(e: KeyboardEvent, id: number, side: "left" | "right"): void {
    const pin = manual.pins.find((p) => p.id === id);
    if (!pin) return;
    const cur = side === "left" ? pin.left_px : pin.right_px;
    const step = e.shiftKey ? 10 : 1;
    let next: [number, number] | null = null;
    if (e.key === "ArrowLeft") next = [cur[0] - step, cur[1]];
    else if (e.key === "ArrowRight") next = [cur[0] + step, cur[1]];
    else if (e.key === "ArrowUp") next = [cur[0], cur[1] - step];
    else if (e.key === "ArrowDown") next = [cur[0], cur[1] + step];
    else if (e.key === "Delete" || e.key === "Backspace") {
      void manual.removePin(id);
      e.preventDefault();
      return;
    } else {
      return;
    }
    if (next) {
      void manual.movePin(id, side, next);
      e.preventDefault();
    }
  }

  // --- Drawing ----------------------------------------------------------------

  /** Paint one preview into the canvas, fail-closed on a malformed payload. */
  function paintInto(
    ctx: CanvasRenderingContext2D,
    preview: ManualPreview | null,
    x: number,
    w: number,
    h: number,
  ): boolean {
    if (!preview) return false;
    if (w === 0 || h === 0) return false;
    if (preview.rgba.length !== w * h * 4) return false;
    ctx.putImageData(
      new ImageData(new Uint8ClampedArray(preview.rgba), w, h),
      x,
      0,
    );
    return true;
  }

  function marker(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    color: string,
    active: boolean,
  ): void {
    ctx.beginPath();
    ctx.arc(x, y, active ? 6 : 4, 0, Math.PI * 2);
    ctx.fillStyle = color;
    ctx.fill();
    if (active) {
      ctx.lineWidth = 2;
      ctx.strokeStyle = color;
      ctx.beginPath();
      ctx.arc(x, y, 9, 0, Math.PI * 2);
      ctx.stroke();
    }
  }

  function draw(): void {
    const el = canvasEl;
    if (!el) return;
    const ctx = el.getContext("2d");
    if (!ctx) return;

    const { totalW, totalH, lw, lh, rw, rh, leftX, rightX } = geom;
    if (el.width !== totalW) el.width = totalW;
    if (el.height !== totalH) el.height = totalH;
    ctx.clearRect(0, 0, totalW, totalH);

    const styles = getComputedStyle(el);
    const dominant =
      styles.getPropertyValue("--color-dominant").trim() || "#1e1e1e";
    const accent = styles.getPropertyValue("--color-accent").trim() || "#4a9eff";
    const warn = styles.getPropertyValue("--color-log-warn").trim() || "#e0a02e";
    const success =
      styles.getPropertyValue("--color-success").trim() || "#3fb950";

    ctx.fillStyle = dominant;
    ctx.fillRect(0, 0, totalW, totalH);

    paintInto(ctx, manual.previewLeft, leftX, lw, lh);
    paintInto(ctx, manual.previewRight, rightX, rw, rh);

    // Verified under unverified so the active/accent ring stays visible.
    for (const pin of displayPins) {
      const active = pin.id === activeId;
      const color = pin.verified ? success : warn;
      marker(ctx, leftX + pin.left_px[0], pin.left_px[1], active ? accent : color, active);
      marker(
        ctx,
        rightX + pin.right_px[0],
        pin.right_px[1],
        active ? accent : color,
        active,
      );
    }

    if (pendingLeft !== null) {
      marker(ctx, leftX + pendingLeft[0], pendingLeft[1], accent, true);
    }
  }

  $effect(() => {
    // Track every input the drawing depends on.
    void manual.previewLeft;
    void manual.previewRight;
    void displayPins;
    void activeId;
    void pendingLeft;
    void canvasEl;
    draw();
  });

  const ariaLabel = $derived(
    `Pin canvas, ${manual.pins.length} pin${manual.pins.length === 1 ? "" : "s"}. ` +
      (pairing
        ? "Now click the matching point on the right frame."
        : "Click a point on the left frame, then its match on the right."),
  );
</script>

<div class="pin-canvas-wrap" role="application" aria-label={ariaLabel}>
  <canvas
    bind:this={canvasEl}
    class="pin-canvas"
    aria-hidden="true"
    onpointerdown={onCanvasPointerDown}
    onpointermove={onCanvasPointerMove}
    onpointerup={onCanvasPointerUp}
    onpointercancel={onCanvasPointerUp}
    onkeydown={onCanvasKeyDown}
    oncontextmenu={(e) => e.preventDefault()}
    tabindex="0"
  ></canvas>

  <!-- Keyboard-focusable pin handles (the accessible editing affordance). -->
  {#each displayPins as pin (pin.id)}
    {@const lq = { x: geom.leftX + pin.left_px[0], y: pin.left_px[1] }}
    {@const rq = { x: geom.rightX + pin.right_px[0], y: pin.right_px[1] }}
    <button
      type="button"
      class="pin-handle"
      style="left: {(lq.x / geom.totalW) * 100}%; top: {(lq.y / geom.totalH) * 100}%"
      aria-label="pin {pin.id + 1}, left"
      onpointerdown={(e) => onHandlePointerDown(e, pin.id, "left")}
      onkeydown={(e) => onHandleKeyDown(e, pin.id, "left")}
    >
      <span class="pin-handle-dot" aria-hidden="true"></span>
    </button>
    <button
      type="button"
      class="pin-handle"
      style="left: {(rq.x / geom.totalW) * 100}%; top: {(rq.y / geom.totalH) * 100}%"
      aria-label="pin {pin.id + 1}, right"
      onpointerdown={(e) => onHandlePointerDown(e, pin.id, "right")}
      onkeydown={(e) => onHandleKeyDown(e, pin.id, "right")}
    >
      <span class="pin-handle-dot" aria-hidden="true"></span>
    </button>
  {/each}
</div>

<style>
  .pin-canvas-wrap {
    position: relative;
    display: block;
    min-width: 0;
    max-width: 100%;
  }

  .pin-canvas {
    display: block;
    width: 100%;
    height: auto;
    background: var(--color-dominant);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    touch-action: none;
    cursor: crosshair;
  }

  .pin-canvas:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .pin-handle {
    position: absolute;
    width: 32px;
    height: 32px;
    margin: -16px 0 0 -16px;
    padding: 0;
    border: none;
    background: transparent;
    cursor: grab;
  }

  .pin-handle:focus-visible {
    outline: none;
  }

  .pin-handle-dot {
    display: block;
    width: 10px;
    height: 10px;
    margin: 11px auto;
    border: 2px solid var(--color-accent);
    border-radius: 50%;
    background: transparent;
  }

  .pin-handle:focus-visible .pin-handle-dot {
    box-shadow: 0 0 0 2px var(--color-accent);
  }
</style>
