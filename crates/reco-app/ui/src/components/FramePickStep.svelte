<!--
  Frame step (MANU-03): choose the reference frame and see it rendered under
  real CameraParams. A frame scrubber (Timeline idiom) with a `Frame i of n`
  readout and a `Use this frame` CTA. The preview canvases paint the RGBA bytes
  the worker produced (readback-canvas idiom: validate header/length, resize
  backing store only on change, fail closed). Scrubbing posts a debounced
  `manual_set_frame`; the canvas repaints on the resulting binary preview frame.
-->
<script lang="ts">
  import { onDestroy } from "svelte";
  import { manual, type ManualPreview } from "../lib/manual.svelte";
  import ActionButton from "./ActionButton.svelte";

  /** The frame shown while a debounced seek is in flight. */
  let displayFrame = $state(0);
  /** The debounce timer for scrub-driven seeks. */
  let pendingSeek: ReturnType<typeof setTimeout> | null = null;

  let canvasLeft = $state<HTMLCanvasElement | null>(null);
  let canvasRight = $state<HTMLCanvasElement | null>(null);

  // The worker is authoritative for the committed frame; mirror it when not
  // mid-scrub.
  $effect(() => {
    if (pendingSeek === null) displayFrame = manual.frame;
  });

  const framesTotal = $derived(manual.framesTotal);
  const max = $derived(framesTotal > 0 ? framesTotal - 1 : 0);
  const hasPreview = $derived(manual.previewLeft !== null);

  function handleInput(e: Event): void {
    const value = Number((e.target as HTMLInputElement).value);
    displayFrame = value;
    if (pendingSeek !== null) clearTimeout(pendingSeek);
    // Debounce so a drag posts one seek once the pointer settles (RESEARCH
    // Part 4 — the preview is instant, the extraction is not free).
    pendingSeek = setTimeout(() => {
      pendingSeek = null;
      void manual.setFrame(value);
    }, 150);
  }

  /**
   * Paint one camera's preview onto its canvas (PreviewSurface idiom).
   *
   * Fails closed on a malformed payload — a zero dimension or a length that
   * does not match `width * height * 4` — so a bad event cannot throw here.
   */
  function paint(
    canvas: HTMLCanvasElement | null,
    preview: ManualPreview | null,
  ): void {
    if (!canvas || !preview) return;
    const { rgba, width, height } = preview;
    if (width === 0 || height === 0) return;
    if (rgba.length !== width * height * 4) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    // Assign only on change: a same-size assignment clears the 2D context.
    if (canvas.width !== width) canvas.width = width;
    if (canvas.height !== height) canvas.height = height;
    const pixels = new Uint8ClampedArray(rgba);
    const imageData = new ImageData(pixels, width, height);
    ctx.putImageData(imageData, 0, 0);
  }

  $effect(() => {
    paint(canvasLeft, manual.previewLeft);
  });

  $effect(() => {
    paint(canvasRight, manual.previewRight);
  });

  onDestroy(() => {
    if (pendingSeek !== null) clearTimeout(pendingSeek);
  });
</script>

<section class="frame-step" aria-label="Reference frame">
  <h3 class="step-heading">Frame</h3>

  <div class="scrubber-row">
    <input
      type="range"
      class="scrubber"
      min="0"
      {max}
      step="1"
      value={displayFrame}
      disabled={framesTotal === 0}
      aria-label="Reference frame"
      aria-valuetext="{displayFrame + 1} of {framesTotal}"
      oninput={handleInput}
    />
    <span class="frame-readout">
      Frame {displayFrame + 1} of {framesTotal || 0}
    </span>
  </div>

  {#if hasPreview}
    <div class="preview-pair">
      <figure class="preview-figure">
        <canvas bind:this={canvasLeft} class="preview-canvas"></canvas>
        <figcaption class="preview-caption">Left</figcaption>
      </figure>
      <figure class="preview-figure">
        <canvas bind:this={canvasRight} class="preview-canvas"></canvas>
        <figcaption class="preview-caption">Right</figcaption>
      </figure>
    </div>
  {:else}
    <p class="empty-hint">
      Scrub to a frame where both cameras see shared content.
    </p>
  {/if}

  <div class="frame-actions">
    <ActionButton
      variant="primary"
      disabled={!hasPreview}
      onClick={() => manual.next()}
    >
      Use this frame
    </ActionButton>
  </div>
</section>

<style>
  .frame-step {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .step-heading {
    margin: 0;
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .scrubber-row {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .scrubber {
    flex: 1;
    height: 4px;
    -webkit-appearance: none;
    appearance: none;
    background: var(--color-dominant);
    border-radius: 2px;
    cursor: pointer;
    outline: none;
  }

  .scrubber:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .scrubber::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    cursor: pointer;
  }

  .scrubber::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    border: none;
    cursor: pointer;
  }

  .scrubber:focus-visible {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .frame-readout {
    font-family: var(--font-mono);
    color: var(--color-body-text);
    white-space: nowrap;
  }

  .preview-pair {
    display: flex;
    gap: var(--space-md);
    flex-wrap: wrap;
  }

  .preview-figure {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    flex: 1 1 280px;
    min-width: 0;
  }

  .preview-canvas {
    width: 100%;
    height: auto;
    aspect-ratio: 16 / 9;
    object-fit: contain;
    background: var(--color-dominant);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
  }

  .preview-caption {
    font-family: var(--font-mono);
    font-size: var(--text-label);
    color: var(--color-log-info);
  }

  .empty-hint {
    margin: 0;
    color: var(--color-log-info);
  }

  .frame-actions {
    display: flex;
    gap: var(--space-md);
  }
</style>
