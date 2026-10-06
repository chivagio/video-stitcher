<!--
  Field ROI editor (CALB-09 / E7). A webview-owned canvas editing surface that
  overlays the paired source-frame thumbnails with the active camera's field ROI
  polygon. The operator can drag a vertex, click an edge to add a vertex, remove a
  vertex (right-click / Alt-click / Delete), move the whole polygon, and Reset or
  Clear. The canvas never hit-tests the Rust-owned native child view: this
  surface is webview-owned and renders its own (downscaled) frames — see
  04-UI-SPEC "roi-overlay-over-native-preview".

  Coordinates are normalized `[0,1]` per camera and reuse the engine's
  `FieldRoi { left, right }` shape. Handles are real, keyboard-focusable buttons
  with an `aria-label` "vertex n of camera A/B"; Escape cancels an in-progress
  drag.
-->
<script lang="ts">
  import type { DebugReport } from "../lib/types";
  import { clamp01, fieldRoi, isPolygon, type Vertex } from "../lib/roi.svelte";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";
  import InlineNotice from "./InlineNotice.svelte";

  let { report = null }: { report?: DebugReport | null } = $props();

  let canvasEl = $state<HTMLCanvasElement | null>(null);
  /** When on, dragging the polygon body moves the whole polygon. */
  let moveMode = $state(false);

  /** Gap in intrinsic canvas pixels between the two frames. */
  const GAP = 4;
  /** Vertex hit radius in intrinsic pixels (the 32px target is the button). */
  const HANDLE_HIT_PX = 16;
  /** Edge-add tolerance in intrinsic pixels. */
  const EDGE_HIT_PX = 10;
  /** Fallback frame size when no thumbnail geometry is available. */
  const FALLBACK_W = 320;
  const FALLBACK_H = 180;

  interface Rect {
    x: number;
    y: number;
    w: number;
    h: number;
  }

  /** The frame geometry in intrinsic canvas pixels. */
  const geom = $derived.by(() => {
    const lw = report && report.left_width > 0 ? report.left_width : FALLBACK_W;
    const lh =
      report && report.left_height > 0 ? report.left_height : FALLBACK_H;
    const rw =
      report && report.right_width > 0 ? report.right_width : FALLBACK_W;
    const rh =
      report && report.right_height > 0 ? report.right_height : FALLBACK_H;
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

  /** The active camera's frame rect within the canvas. */
  const activeRect = $derived<Rect>(
    fieldRoi.activeCamera === "left"
      ? { x: geom.leftX, y: 0, w: geom.lw, h: geom.lh }
      : { x: geom.rightX, y: 0, w: geom.rw, h: geom.rh },
  );

  const activeVertices = $derived(fieldRoi.active);
  const activeIsPolygon = $derived(isPolygon(activeVertices));
  const cameraLabel = $derived(fieldRoi.activeCamera === "left" ? "A" : "B");

  // --- Geometry helpers -------------------------------------------------------

  function vertexPx(v: Vertex): { x: number; y: number } {
    const r = activeRect;
    return { x: r.x + v[0] * r.w, y: r.y + v[1] * r.h };
  }

  /** The pointer position in intrinsic canvas pixels. */
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

  /** The intrinsic→CSS scale (used to keep hit tolerances screen-sized). */
  function displayScale(): number {
    const el = canvasEl;
    if (!el) return 1;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 ? geom.totalW / rect.width : 1;
  }

  /** Convert an intrinsic pixel position to normalized active-frame coords. */
  function toNormalized(p: { x: number; y: number }): Vertex {
    const r = activeRect;
    return [clamp01((p.x - r.x) / r.w), clamp01((p.y - r.y) / r.h)];
  }

  /** The index of the active vertex under `p`, or -1. */
  function hitVertex(p: { x: number; y: number }): number {
    const tol = HANDLE_HIT_PX * displayScale();
    let best = -1;
    let bestDist = tol;
    activeVertices.forEach((v, i) => {
      const q = vertexPx(v);
      const d = Math.hypot(q.x - p.x, q.y - p.y);
      if (d <= bestDist) {
        bestDist = d;
        best = i;
      }
    });
    return best;
  }

  function projectOnSegment(
    p: { x: number; y: number },
    a: { x: number; y: number },
    b: { x: number; y: number },
  ): { dist: number; x: number; y: number } {
    const dx = b.x - a.x;
    const dy = b.y - a.y;
    const len2 = dx * dx + dy * dy;
    let t = len2 > 0 ? ((p.x - a.x) * dx + (p.y - a.y) * dy) / len2 : 0;
    t = Math.max(0, Math.min(1, t));
    const x = a.x + t * dx;
    const y = a.y + t * dy;
    return { dist: Math.hypot(p.x - x, p.y - y), x, y };
  }

  /** The edge (start-vertex index) under `p`, or null. */
  function hitEdge(p: { x: number; y: number }): number | null {
    if (activeVertices.length < 2) return null;
    const tol = EDGE_HIT_PX * displayScale();
    let bestIndex: number | null = null;
    let bestDist = tol;
    for (let i = 0; i < activeVertices.length; i++) {
      const a = vertexPx(activeVertices[i]);
      const b = vertexPx(activeVertices[(i + 1) % activeVertices.length]);
      const proj = projectOnSegment(p, a, b);
      if (proj.dist <= bestDist) {
        bestDist = proj.dist;
        bestIndex = i;
      }
    }
    return bestIndex;
  }

  function pointInPolygon(p: { x: number; y: number }, verts: Vertex[]): boolean {
    let inside = false;
    for (let i = 0, j = verts.length - 1; i < verts.length; j = i++) {
      const [xi, yi] = verts[i];
      const [xj, yj] = verts[j];
      const intersects =
        yi > p.y !== yj > p.y &&
        p.x < ((xj - xi) * (p.y - yi)) / (yj - yi) + xi;
      if (intersects) inside = !inside;
    }
    return inside;
  }

  // --- Drag state -------------------------------------------------------------

  type Drag =
    | { kind: "vertex"; index: number }
    | { kind: "polygon"; lastX: number; lastY: number };

  let drag = $state<Drag | null>(null);
  /** The active polygon snapshot before a drag, for Escape. */
  let dragBackup: Vertex[] | null = null;

  function startDrag(next: Drag, e: PointerEvent): void {
    drag = next;
    dragBackup = activeVertices.map((v) => [v[0], v[1]] as Vertex);
    canvasEl?.setPointerCapture(e.pointerId);
  }

  function endDrag(e: PointerEvent): void {
    if (drag === null) return;
    drag = null;
    dragBackup = null;
    if (canvasEl?.hasPointerCapture(e.pointerId)) {
      canvasEl.releasePointerCapture(e.pointerId);
    }
  }

  function onCanvasPointerDown(e: PointerEvent): void {
    const p = pointerPx(e);
    if (!p) return;
    // Remove a vertex: right-click or Alt-click on a handle.
    if (e.button === 2 || e.altKey) {
      const idx = hitVertex(p);
      if (idx >= 0) fieldRoi.removeVertex(idx);
      return;
    }
    if (e.button !== 0) return;

    const idx = hitVertex(p);
    if (idx >= 0) {
      startDrag({ kind: "vertex", index: idx }, e);
      return;
    }
    const normalized = toNormalized(p);
    if (
      isPolygon(activeVertices) &&
      (moveMode ||
        pointInPolygon({ x: normalized[0], y: normalized[1] }, activeVertices))
    ) {
      // Inside the polygon (or in Move mode): translate the whole polygon.
      startDrag({ kind: "polygon", lastX: e.clientX, lastY: e.clientY }, e);
      return;
    }
    // Near an edge: splice a new vertex onto that edge. Otherwise append one.
    const edge = hitEdge(p);
    if (edge !== null) fieldRoi.addVertex(normalized[0], normalized[1], edge + 1);
    else fieldRoi.addVertex(normalized[0], normalized[1]);
  }

  function onCanvasPointerMove(e: PointerEvent): void {
    if (drag === null) return;
    if (drag.kind === "vertex") {
      const p = pointerPx(e);
      if (!p) return;
      const n = toNormalized(p);
      fieldRoi.moveVertex(drag.index, n[0], n[1]);
    } else {
      // The pointer deltas are CSS pixels; convert them to intrinsic canvas
      // pixels (the same scale `pointerPx` uses) before normalizing against the
      // intrinsic frame rect, or a whole-polygon drag would move at
      // renderedWidth/intrinsicWidth of the pointer travel (WR-01).
      const r = activeRect;
      const scale = displayScale();
      const dx = ((e.clientX - drag.lastX) * scale) / r.w;
      const dy = ((e.clientY - drag.lastY) * scale) / r.h;
      drag = { kind: "polygon", lastX: e.clientX, lastY: e.clientY };
      fieldRoi.movePolygon(dx, dy);
    }
  }

  function onCanvasPointerUp(e: PointerEvent): void {
    endDrag(e);
  }

  function onEditorKeyDown(e: KeyboardEvent): void {
    if (e.key === "Escape" && drag !== null) {
      if (dragBackup !== null) fieldRoi.restore(dragBackup);
      drag = null;
      dragBackup = null;
      e.preventDefault();
    }
  }

  // --- Handle (keyboard-focusable vertex) interactions ------------------------

  function onHandlePointerDown(e: PointerEvent, index: number): void {
    e.stopPropagation();
    if (e.button === 2 || e.altKey) {
      fieldRoi.removeVertex(index);
      return;
    }
    if (e.button !== 0) return;
    startDrag({ kind: "vertex", index }, e);
  }

  function onHandleKeyDown(e: KeyboardEvent, index: number): void {
    const v = activeVertices[index];
    if (!v) return;
    const step = e.shiftKey ? 0.02 : 0.005;
    if (e.key === "ArrowLeft") {
      fieldRoi.moveVertex(index, v[0] - step, v[1]);
    } else if (e.key === "ArrowRight") {
      fieldRoi.moveVertex(index, v[0] + step, v[1]);
    } else if (e.key === "ArrowUp") {
      fieldRoi.moveVertex(index, v[0], v[1] - step);
    } else if (e.key === "ArrowDown") {
      fieldRoi.moveVertex(index, v[0], v[1] + step);
    } else if (e.key === "Delete" || e.key === "Backspace") {
      fieldRoi.removeVertex(index);
    } else {
      return;
    }
    e.preventDefault();
  }

  // --- Action buttons ---------------------------------------------------------

  function handleAddPoint(): void {
    const verts = activeVertices;
    if (verts.length === 0) {
      fieldRoi.addVertex(0.5, 0.5);
    } else if (verts.length === 1) {
      fieldRoi.addVertex(verts[0][0], clamp01(verts[0][1] + 0.1));
    } else {
      // Append the midpoint of the closing edge — a deterministic visible point.
      const a = verts[verts.length - 1];
      const b = verts[0];
      fieldRoi.addVertex((a[0] + b[0]) / 2, (a[1] + b[1]) / 2);
    }
  }

  function handleRemovePoint(): void {
    if (activeVertices.length > 0) fieldRoi.removeVertex(activeVertices.length - 1);
  }

  // --- Drawing ----------------------------------------------------------------

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
    const muted =
      styles.getPropertyValue("--color-log-info").trim() || "#c8c8c8";

    ctx.fillStyle = dominant;
    ctx.fillRect(0, 0, totalW, totalH);

    const leftOk =
      report !== null &&
      report.left_width > 0 &&
      report.left_height > 0 &&
      report.left_thumb.length === report.left_width * report.left_height * 4;
    const rightOk =
      report !== null &&
      report.right_width > 0 &&
      report.right_height > 0 &&
      report.right_thumb.length === report.right_width * report.right_height * 4;

    if (leftOk && report) {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(report.left_thumb), lw, lh),
        leftX,
        0,
      );
    }
    if (rightOk && report) {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(report.right_thumb), rw, rh),
        rightX,
        0,
      );
    }

    // Outline the editable frame so the active camera is unambiguous.
    ctx.save();
    ctx.setLineDash([6, 4]);
    ctx.strokeStyle = accent;
    ctx.globalAlpha = 0.5;
    ctx.lineWidth = 1;
    ctx.strokeRect(activeRect.x + 0.5, 0.5, activeRect.w - 1, activeRect.h - 1);
    ctx.restore();

    const strokePolygon = (
      verts: Vertex[],
      rect: Rect,
      color: string,
      closed: boolean,
    ): void => {
      if (verts.length === 0) return;
      ctx.beginPath();
      verts.forEach((v, i) => {
        const x = rect.x + v[0] * rect.w;
        const y = rect.y + v[1] * rect.h;
        if (i === 0) ctx.moveTo(x, y);
        else ctx.lineTo(x, y);
      });
      if (closed) ctx.closePath();
      ctx.strokeStyle = color;
      ctx.lineWidth = 2;
      ctx.stroke();
    };

    // Inactive camera polygon: muted, no handles.
    if (fieldRoi.activeCamera === "left") {
      strokePolygon(
        fieldRoi.right,
        { x: rightX, y: 0, w: rw, h: rh },
        muted,
        isPolygon(fieldRoi.right),
      );
    } else {
      strokePolygon(
        fieldRoi.left,
        { x: leftX, y: 0, w: lw, h: lh },
        muted,
        isPolygon(fieldRoi.left),
      );
    }

    // Active camera polygon: accent outline (closed only when it is a polygon).
    strokePolygon(activeVertices, activeRect, accent, activeIsPolygon);

    // Active vertex dots (the buttons overlay these for interaction).
    ctx.fillStyle = accent;
    for (const v of activeVertices) {
      const q = vertexPx(v);
      ctx.beginPath();
      ctx.arc(q.x, q.y, 4, 0, Math.PI * 2);
      ctx.fill();
    }
  }

  $effect(() => {
    // Track every input the drawing depends on.
    void report;
    void canvasEl;
    void fieldRoi.activeCamera;
    void fieldRoi.left;
    void fieldRoi.right;
    void activeRect;
    draw();
  });

  const ariaLabel = $derived(
    `Field ROI editor, camera ${cameraLabel}: ` +
      `${activeVertices.length} vertex/vertices`,
  );
</script>

<svelte:window onkeydown={onEditorKeyDown} />

<div class="roi-editor">
  <div class="roi-header">
    <span class="roi-title"><Icon name="polygon" /> Field ROI</span>
    <div class="camera-toggle" role="group" aria-label="Camera">
      <button
        type="button"
        class="camera-btn"
        class:active={fieldRoi.activeCamera === "left"}
        aria-pressed={fieldRoi.activeCamera === "left"}
        onclick={() => fieldRoi.setCamera("left")}
      >
        Camera A
      </button>
      <button
        type="button"
        class="camera-btn"
        class:active={fieldRoi.activeCamera === "right"}
        aria-pressed={fieldRoi.activeCamera === "right"}
        onclick={() => fieldRoi.setCamera("right")}
      >
        Camera B
      </button>
    </div>
  </div>

  <div class="roi-canvas-wrap" role="application" aria-label={ariaLabel}>
    <canvas
      bind:this={canvasEl}
      class="roi-canvas"
      aria-hidden="true"
      onpointerdown={onCanvasPointerDown}
      onpointermove={onCanvasPointerMove}
      onpointerup={onCanvasPointerUp}
      onpointercancel={onCanvasPointerUp}
      oncontextmenu={(e) => e.preventDefault()}
    ></canvas>

    {#if activeVertices.length === 0}
      <p class="canvas-hint">Draw a field polygon — click the frame to add points.</p>
    {:else if !activeIsPolygon}
      <p class="canvas-hint">
        A polygon needs at least 3 points; saving will clear it.
      </p>
    {/if}

    <!-- Keyboard-focusable vertex handles (the accessible editing affordance). -->
    {#each activeVertices as v, i (i)}
      {@const q = vertexPx(v)}
      <button
        type="button"
        class="roi-handle"
        style="left: {(q.x / geom.totalW) * 100}%; top: {(q.y / geom.totalH) * 100}%"
        aria-label="vertex {i + 1} of camera {cameraLabel}"
        onpointerdown={(e) => onHandlePointerDown(e, i)}
        onkeydown={(e) => onHandleKeyDown(e, i)}
      >
        <span class="roi-handle-dot" aria-hidden="true"></span>
      </button>
    {/each}
  </div>

  <div class="roi-actions">
    <ActionButton variant="secondary" onClick={handleAddPoint}>
      <Icon name="plus" /> Add point
    </ActionButton>
    <ActionButton
      variant="secondary"
      disabled={activeVertices.length === 0}
      onClick={handleRemovePoint}
    >
      <Icon name="trash" /> Remove point
    </ActionButton>
    <ActionButton
      variant={moveMode ? "primary" : "secondary"}
      title="Toggle whole-polygon move"
      onClick={() => (moveMode = !moveMode)}
    >
      <Icon name="move" /> Move polygon
    </ActionButton>
    <ActionButton variant="secondary" onClick={() => fieldRoi.reset()}>
      <Icon name="reset" /> Reset polygon
    </ActionButton>
    <ActionButton variant="secondary" onClick={() => fieldRoi.clear()}>
      <Icon name="close" /> Clear polygon
    </ActionButton>
    <ActionButton variant="primary" onClick={() => void fieldRoi.apply()}>
      <Icon name="save" /> Save field ROI
    </ActionButton>
  </div>

  {#if fieldRoi.saved}
    <InlineNotice level="info" message="Field ROI saved to the calibration profile." />
  {/if}
  {#if fieldRoi.error !== null}
    <InlineNotice level="error" message={fieldRoi.error} />
  {/if}
</div>

<style>
  .roi-editor {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
    min-width: 0;
  }

  .roi-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-sm);
    flex-wrap: wrap;
  }

  .roi-title {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .camera-toggle {
    display: inline-flex;
    gap: var(--space-xs);
  }

  .camera-btn {
    min-height: 32px;
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .camera-btn.active {
    border-color: var(--color-accent);
    color: var(--color-accent);
  }

  .camera-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .roi-canvas-wrap {
    position: relative;
    display: block;
    width: 100%;
    background: var(--color-dominant);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    overflow: hidden;
  }

  .roi-canvas {
    display: block;
    width: 100%;
    height: auto;
    touch-action: none;
    cursor: crosshair;
  }

  .canvas-hint {
    position: absolute;
    left: 50%;
    top: 50%;
    transform: translate(-50%, -50%);
    margin: 0;
    max-width: 80%;
    text-align: center;
    color: var(--color-log-info);
    font-size: var(--text-body);
    pointer-events: none;
  }

  .roi-handle {
    position: absolute;
    width: 32px;
    height: 32px;
    margin: 0;
    padding: 0;
    transform: translate(-50%, -50%);
    border: none;
    background: transparent;
    cursor: grab;
    touch-action: none;
  }

  .roi-handle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
    border-radius: 50%;
  }

  .roi-handle-dot {
    display: block;
    width: 10px;
    height: 10px;
    margin: 0 auto;
    border-radius: 50%;
    background: var(--color-accent);
    border: 1px solid var(--color-dominant);
  }

  .roi-actions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-sm);
  }
</style>
