<!--
  Residual map (CALB-08 / E5b). Draws the downscaled undistorted thumbnails and
  colours each match point by its reprojection error on a sequential single-hue
  ramp (cold = low, hot = high) over the accent hue, with a numeric legend. The
  legend states the values, so colour is never the only signal. Fails closed on
  a malformed payload (T-04-11).
-->
<script lang="ts">
  import type { DebugPoint, DebugReport } from "../lib/types";

  let { report }: { report: DebugReport } = $props();

  let canvasEl = $state<HTMLCanvasElement | null>(null);

  const GAP = 4;
  const RADIUS = 3;

  // Sequential single-hue ramp endpoints on the accent hue (declared once):
  // cold = a dim blue, hot = the accent. Interpolated per point.
  const COLD: [number, number, number] = [30, 58, 95];
  const HOT: [number, number, number] = [74, 158, 255];

  function rampColor(t: number): string {
    const c = COLD.map((v, i) => Math.round(v + (HOT[i] - v) * t));
    return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
  }

  const allPoints = $derived([...report.verified, ...report.rejected]);
  const errorRange = $derived.by(() => {
    const errors = allPoints.map((p) => p.error).filter((e) => Number.isFinite(e));
    if (errors.length === 0) return { low: 0, high: 0 };
    return { low: Math.min(...errors), high: Math.max(...errors) };
  });

  const ariaLabel = $derived(
    `Residual map, frame ${report.frame_index + 1} of ${report.frames_total}: ` +
      `${allPoints.length} points, error ${errorRange.low.toFixed(3)} to ${errorRange.high.toFixed(3)}`,
  );

  function dot(ctx: CanvasRenderingContext2D, x: number, y: number): void {
    ctx.beginPath();
    ctx.arc(x, y, RADIUS, 0, Math.PI * 2);
    ctx.fill();
  }

  function draw(): void {
    const el = canvasEl;
    if (!el) return;
    const ctx = el.getContext("2d");
    if (!ctx) return;

    const lw = report.left_width;
    const lh = report.left_height;
    const rw = report.right_width;
    const rh = report.right_height;
    const leftOk = lw > 0 && lh > 0 && report.left_thumb.length === lw * lh * 4;
    const rightOk = rw > 0 && rh > 0 && report.right_thumb.length === rw * rh * 4;
    if (!leftOk && !rightOk) {
      if (el.width !== 0) el.width = 0;
      if (el.height !== 0) el.height = 0;
      return;
    }

    const height = Math.max(leftOk ? lh : 0, rightOk ? rh : 0);
    const width =
      (leftOk ? lw : 0) + (rightOk ? rw : 0) + (leftOk && rightOk ? GAP : 0);
    if (el.width !== width) el.width = width;
    if (el.height !== height) el.height = height;
    ctx.clearRect(0, 0, width, height);

    const leftX = 0;
    const rightX = leftOk ? lw + GAP : 0;

    if (leftOk) {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(report.left_thumb), lw, lh),
        leftX,
        0,
      );
    }
    if (rightOk) {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(report.right_thumb), rw, rh),
        rightX,
        0,
      );
    }

    const { low, high } = errorRange;
    const span = high - low;
    const colorFor = (p: DebugPoint): string => {
      if (!Number.isFinite(p.error) || span <= 0) return rampColor(0);
      const t = Math.min(1, Math.max(0, (p.error - low) / span));
      return rampColor(t);
    };

    for (const p of allPoints) {
      ctx.fillStyle = colorFor(p);
      if (leftOk) dot(ctx, leftX + p.x_nx * lw, p.y_nx * lh);
      if (rightOk) dot(ctx, rightX + p.x_nx * rw, p.y_nx * rh);
    }
  }

  $effect(() => {
    void report;
    void canvasEl;
    draw();
  });
</script>

<!-- `role="img"` lives on the wrapper (the PreviewSurface idiom): a `<canvas>`
     cannot take a non-interactive role, and the wrapper carries the summary. -->
<div class="residual-map" role="img" aria-label={ariaLabel}>
  <canvas bind:this={canvasEl} class="map-canvas" aria-hidden="true"></canvas>
  <div class="legend">
    <span class="legend-label">low</span>
    <span
      class="ramp"
      style="background: linear-gradient(to right, rgb({COLD[0]}, {COLD[1]}, {COLD[2]}), rgb({HOT[0]}, {HOT[1]}, {HOT[2]}))"
    ></span>
    <span class="legend-label">high</span>
    <span class="legend-values">
      {errorRange.low.toFixed(3)} … {errorRange.high.toFixed(3)}
    </span>
  </div>
</div>

<style>
  .residual-map {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    min-width: 0;
  }

  .map-canvas {
    display: block;
    max-width: 100%;
    height: auto;
    background: var(--color-dominant);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
  }

  .legend {
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    font-size: var(--text-body);
    color: var(--color-log-info);
  }

  .legend-label {
    font-weight: var(--weight-semibold);
  }

  .ramp {
    display: inline-block;
    width: 96px;
    height: 8px;
    border-radius: var(--space-xs);
  }

  .legend-values {
    font-family: var(--font-mono);
    margin-left: var(--space-xs);
  }
</style>
