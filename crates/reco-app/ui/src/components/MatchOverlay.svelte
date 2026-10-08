<!--
  Feature-match overlay (CALB-08 / E5). Draws the downscaled undistorted left
  and right thumbnails side by side and overlays the verified matches (accent)
  and rejected candidates (warn) as markers. The canvas sizes from the report's
  own geometry and fails closed on a malformed payload — it never throws inside
  the uncaught event handler (T-04-11). Colours come from the design tokens.
-->
<script lang="ts">
  import type { DebugPoint, DebugReport } from "../lib/types";

  let { report }: { report: DebugReport } = $props();

  let canvasEl = $state<HTMLCanvasElement | null>(null);

  /** Gap in pixels between the two thumbnails. */
  const GAP = 4;
  /** Marker radius in intrinsic canvas pixels. */
  const RADIUS = 2.5;

  const ariaLabel = $derived(
    `Feature-match overlay, frame ${report.frame_index + 1} of ${report.frames_total}: ` +
      `${report.verified.length} verified, ${report.rejected.length} rejected matches`,
  );

  function marker(ctx: CanvasRenderingContext2D, x: number, y: number): void {
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
    // Fail closed on a malformed payload: a length/geometry mismatch means we
    // cannot safely build ImageData (the prior hard-coded size threw).
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

    const styles = getComputedStyle(el);
    const accent = styles.getPropertyValue("--color-accent").trim() || "#4a9eff";
    const warn = styles.getPropertyValue("--color-log-warn").trim() || "#e0a02e";

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

    const drawClass = (points: DebugPoint[], color: string): void => {
      ctx.fillStyle = color;
      for (const p of points) {
        // Each point carries BOTH cameras' positions: draw the left camera's
        // keypoint on the left thumbnail and the right camera's on the right
        // thumbnail (never mirror one camera's point onto both).
        if (leftOk) marker(ctx, leftX + p.left_x_nx * lw, p.left_y_nx * lh);
        if (rightOk) marker(ctx, rightX + p.right_x_nx * rw, p.right_y_nx * rh);
      }
    };
    // Rejected under verified so surviving matches stay visible on top.
    drawClass(report.rejected, warn);
    drawClass(report.verified, accent);
  }

  $effect(() => {
    // Track the report identity and the canvas binding; redraw on change.
    void report;
    void canvasEl;
    draw();
  });
</script>

<!-- `role="img"` lives on the wrapper (the PreviewSurface idiom): a `<canvas>`
     cannot take a non-interactive role, and the wrapper is what carries the
     summary label for assistive tech. -->
<div class="match-overlay" role="img" aria-label={ariaLabel}>
  <canvas bind:this={canvasEl} class="overlay-canvas" aria-hidden="true"></canvas>
  <div class="legend">
    <span class="swatch verified"></span> verified
    <span class="swatch rejected"></span> rejected
    {#if report.points_capped}
      <span class="capped">showing first {report.verified.length +
          report.rejected.length} points</span>
    {/if}
  </div>
</div>

<style>
  .match-overlay {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    min-width: 0;
  }

  .overlay-canvas {
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
    gap: var(--space-sm);
    font-size: var(--text-body);
    color: var(--color-log-info);
  }

  .swatch {
    display: inline-block;
    width: 10px;
    height: 10px;
    border-radius: 50%;
  }

  .swatch.verified {
    background: var(--color-accent);
  }

  .swatch.rejected {
    background: var(--color-log-warn);
  }

  .capped {
    color: var(--color-log-warn);
  }
</style>
