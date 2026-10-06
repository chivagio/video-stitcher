<!--
  Frame step (MANU-03): choose the reference frame and see it rendered under
  real CameraParams. Minimal shell in plan 04.1-01 task 2; the scrubber and the
  readback preview canvases are added in task 3.
-->
<script lang="ts">
  import { manual } from "../lib/manual.svelte";
  import ActionButton from "./ActionButton.svelte";

  const hasFrame = $derived(manual.previewLeft !== null);
</script>

<section class="frame-step" aria-label="Reference frame">
  <h3 class="step-heading">Frame</h3>
  {#if hasFrame}
    <p class="frame-readout">
      Frame {manual.frame + 1} of {manual.framesTotal || 0}
    </p>
  {:else}
    <p class="empty-hint">
      Scrub to a frame where both cameras see shared content.
    </p>
  {/if}
  <ActionButton variant="primary" disabled={!hasFrame} onClick={() => {}}>
    Use this frame
  </ActionButton>
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
  }

  .frame-readout {
    margin: 0;
    font-family: var(--font-mono);
    color: var(--color-body-text);
  }

  .empty-hint {
    margin: 0;
    color: var(--color-log-info);
  }
</style>
