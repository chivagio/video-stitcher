<!--
  Bend step (MANU-05 / MANU-06): the two handle families over the stitched
  preview. `Lens correction` hosts the on-image lens handles (real
  `CameraParams`: rim → k1, center → cx/cy, explicit scale → fx with fy=fx);
  `Rig alignment` hosts the constrained layout handles (x_ty, x_rz, intersect,
  cam_d). Every drag re-renders the warp instantly under the edited real
  parameters and arms one debounced background re-solve; the resulting layout
  delta is shown, never applied silently. Reset lens / Reset rig restore the
  profile baseline captured at session open. The degeneracy and clamp notes are
  stated once here.
-->
<script lang="ts">
  import { manual, type ManualPreview } from "../lib/manual.svelte";
  import LensHandles from "./LensHandles.svelte";
  import LayoutHandles from "./LayoutHandles.svelte";
  import SolveStatus from "./SolveStatus.svelte";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";

  let canvasLeft = $state<HTMLCanvasElement | null>(null);
  let canvasRight = $state<HTMLCanvasElement | null>(null);

  /** Paint one camera's preview, failing closed on a malformed payload. */
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
    if (canvas.width !== width) canvas.width = width;
    if (canvas.height !== height) canvas.height = height;
    ctx.putImageData(
      new ImageData(new Uint8ClampedArray(rgba), width, height),
      0,
      0,
    );
  }

  $effect(() => {
    paint(canvasLeft, manual.previewLeft);
  });

  $effect(() => {
    paint(canvasRight, manual.previewRight);
  });

  const delta = $derived(manual.layoutDelta);
</script>

<section class="bend-step" aria-label="Bend">
  <div class="bend-head">
    <h3 class="step-heading">Bend</h3>
    <SolveStatus />
  </div>

  <h4 class="section-heading">Lens correction</h4>
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
  <LensHandles />

  <h4 class="section-heading">Rig alignment</h4>
  <LayoutHandles />

  {#if delta}
    <p class="delta-note">
      Re-solve layout delta: cam_d {delta.cam_d >= 0 ? "+" : ""}{delta.cam_d.toFixed(4)} ·
      intersect {delta.intersect >= 0 ? "+" : ""}{delta.intersect.toFixed(4)} ·
      x_ty {delta.x_ty >= 0 ? "+" : ""}{delta.x_ty.toFixed(4)} ·
      x_rz {delta.x_rz >= 0 ? "+" : ""}{delta.x_rz.toFixed(4)}
    </p>
  {/if}

  <p class="degeneracy-note">
    Scale and distance both set apparent zoom; center shift can mimic a rig
    shift — adjust one at a time.
  </p>
  <p class="clamp-note">Handles stop at their safe travel limit.</p>

  <div class="bend-actions">
    <ActionButton variant="secondary" onClick={() => void manual.resetLens()}>
      <Icon name="reset" /> Reset lens
    </ActionButton>
    <ActionButton variant="secondary" onClick={() => void manual.resetRig()}>
      <Icon name="reset" /> Reset rig
    </ActionButton>
    <span class="spacer"></span>
    <ActionButton
      variant="secondary"
      disabled={manual.undoDepth === 0}
      onClick={() => manual.undoHandle()}
    >
      Undo
    </ActionButton>
  </div>
</section>

<style>
  .bend-step {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .bend-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-md);
  }

  .step-heading {
    margin: 0;
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .section-heading {
    margin: 0;
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    color: var(--color-log-info);
    text-transform: uppercase;
    letter-spacing: 0.04em;
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
    flex: 1 1 240px;
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

  .delta-note {
    margin: 0;
    font-family: var(--font-mono);
    font-size: var(--text-label);
    color: var(--color-accent);
  }

  .degeneracy-note,
  .clamp-note {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .bend-actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .spacer {
    flex: 1 1 auto;
  }
</style>
