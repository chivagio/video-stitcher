<!--
  Validation comparison canvas (MANU-07). Paints the stitched output the worker
  produced for one validation frame, and supports:
    - Blink: alternate between the calibration frame's stitched output and the
      validation frame's (user-triggered; auto-alternates unless reduced motion
      is requested — UI-SPEC Accessibility);
    - Blend: a 50/50 composite of the two stitched outputs.
  Readback-canvas idiom (PreviewSurface / FramePickStep): validate header/length
  before painting, resize the backing store only on change, fail closed on a
  malformed payload. The worker is authoritative for the bytes; this component
  never touches the GPU.
-->
<script lang="ts">
  import { manual, type ValidationFrame } from "../lib/manual.svelte";

  let {
    frame,
    mode,
  }: { frame: number; mode: "blink" | "blend" } = $props();

  let canvas = $state<HTMLCanvasElement | null>(null);
  /** Which stitched output Blink is currently showing. */
  let showReference = $state(false);
  let timer: ReturnType<typeof setInterval> | null = null;

  const validation = $derived(manual.validationFrames[frame] ?? null);

  /** Whether the operator requested reduced motion (no auto-animated blink). */
  const reducedMotion =
    typeof window !== "undefined" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  // Auto-alternate only in Blink mode, and never under reduced motion. The
  // interval is cleared on every re-run (mode change) and on destroy.
  $effect(() => {
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
    if (mode === "blink" && !reducedMotion) {
      timer = setInterval(() => {
        showReference = !showReference;
      }, 600);
    } else {
      showReference = false;
    }
    return () => {
      if (timer !== null) {
        clearInterval(timer);
        timer = null;
      }
    };
  });

  /**
   * Paint the comparison, failing closed on a malformed payload.
   *
   * The validation buffer is authoritative: a zero dimension or a length that
   * does not match `width * height * 4` means nothing is painted. The reference
   * buffer is only used when it matches the validation geometry exactly.
   */
  function paint(
    target: HTMLCanvasElement | null,
    vf: ValidationFrame | null,
    comparison: "blink" | "blend",
    reference: boolean,
  ): void {
    if (!target || !vf) return;
    const { rgba, width, height, referenceRgba, referenceWidth, referenceHeight } =
      vf;
    if (width === 0 || height === 0) return;
    if (rgba.length !== width * height * 4) return;
    const refValid =
      referenceWidth === width &&
      referenceHeight === height &&
      referenceRgba.length === width * height * 4;

    const ctx = target.getContext("2d");
    if (!ctx) return;
    if (target.width !== width) target.width = width;
    if (target.height !== height) target.height = height;

    if (comparison === "blend" && refValid) {
      const blended = new Uint8ClampedArray(rgba.length);
      for (let i = 0; i < rgba.length; i += 1) {
        blended[i] = (rgba[i] + referenceRgba[i]) / 2;
      }
      ctx.putImageData(new ImageData(blended, width, height), 0, 0);
    } else if (comparison === "blink" && reference && refValid) {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(referenceRgba), width, height),
        0,
        0,
      );
    } else {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(rgba), width, height),
        0,
        0,
      );
    }
  }

  $effect(() => {
    paint(canvas, validation, mode, showReference);
  });

  const ariaLabel = $derived.by(() => {
    if (!validation) return "No validation frame rendered yet";
    const showing =
      mode === "blend"
        ? "blended calibration and validation frames"
        : showReference
          ? "calibration frame"
          : "validation frame";
    return `Validation frame ${frame + 1}, residual ${validation.residual.toFixed(4)}, showing ${showing}`;
  });
</script>

<!-- The comparison surface is a static painted image with no interactive
     semantics; UI-SPEC Accessibility requires `role="img"` + a descriptive
     `aria-label`. The canvas element carries it (Svelte's generic lint treats
     canvas as potentially interactive). -->
<!-- svelte-ignore a11y_no_interactive_element_to_noninteractive_role -->
<canvas
  bind:this={canvas}
  class="validation-canvas"
  role="img"
  aria-label={ariaLabel}
  title={ariaLabel}
></canvas>

<style>
  .validation-canvas {
    width: 100%;
    height: auto;
    aspect-ratio: 16 / 9;
    object-fit: contain;
    background: var(--color-dominant);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
  }
</style>
