<!--
  Typed lens-refinement readout (INTR-03).

  Renders the engine's `IntrinsicsRefinementView` verbatim: the refined `k1`
  (`old → new`), the held-out residual delta (monospace), the accepted/rejected
  verdict (word PLUS colour), the engine-authored reason, and the
  fixed-parameter note. The readout never re-types an engine value; a missing
  held-out residual renders "Not reported", never a fabricated zero. A rejected
  refinement shows the unchanged baseline `k1` and the reason via `InlineNotice`
  (the safety contract, visible to the user).
-->
<script lang="ts">
  import type { IntrinsicsRefinementView } from "../lib/types";
  import InlineNotice from "./InlineNotice.svelte";
  import Icon from "./Icon.svelte";

  let { refinement }: { refinement: IntrinsicsRefinementView } = $props();

  const heading = $derived(
    refinement.accepted ? "Lens refined (k1)" : "Lens not refined",
  );
  const verdictWord = $derived(refinement.accepted ? "Accepted" : "Rejected");

  /** `k1 old → new` (monospace, 4 decimals). */
  const k1Range = $derived(
    `${refinement.baseline_k1.toFixed(4)} → ${refinement.k1.toFixed(4)}`,
  );

  /** The held-out residual delta, or null when it was never evaluated. */
  const residualRange = $derived.by(() => {
    const before = refinement.heldout_baseline;
    const after = refinement.heldout_refined;
    if (before === null || after === null) return null;
    return `${before.toFixed(6)} → ${after.toFixed(6)}`;
  });
</script>

<section class="intrinsics-readout" aria-labelledby="intrinsics-readout-heading">
  <h3
    id="intrinsics-readout-heading"
    class="heading"
    class:accepted={refinement.accepted}
    class:rejected={!refinement.accepted}
  >
    <Icon name={refinement.accepted ? "check-circle" : "warning"} />
    {heading}
  </h3>

  <dl class="readout" aria-describedby="intrinsics-readout-reason">
    <div class="row">
      <dt class="label">k1</dt>
      <dd class="value mono">{k1Range}</dd>
    </div>
    <div class="row">
      <dt class="label">Held-out residual</dt>
      <dd class="value mono">
        {#if residualRange !== null}
          {residualRange}
        {:else}
          Not reported
        {/if}
      </dd>
    </div>
    <div class="row">
      <dt class="label">Result</dt>
      <dd
        class="value verdict"
        class:accepted={refinement.accepted}
        class:rejected={!refinement.accepted}
      >
        {verdictWord}
      </dd>
    </div>
  </dl>

  {#if !refinement.accepted}
    <div id="intrinsics-readout-reason">
      <InlineNotice level="warn" message={refinement.reason} />
    </div>
  {:else}
    <!-- Symmetric with the rejected branch's `InlineNotice` (`role="status"`):
         an accepted verdict is announced politely rather than arriving silently.
         The two branches are mutually exclusive, so the reason is announced once. -->
    <p id="intrinsics-readout-reason" class="reason" role="status" aria-live="polite">
      {refinement.reason}
    </p>
  {/if}

  <p class="fixed-note">
    Scale, centre, and camera distance are held fixed (they are degenerate with
    the rig).
  </p>
</section>

<style>
  .intrinsics-readout {
    padding: var(--space-lg);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .heading {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    margin: 0;
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
  }

  /* Accepted/rejected is conveyed by word PLUS colour, never colour alone. */
  .heading.accepted,
  .verdict.accepted {
    color: var(--color-success);
  }

  .heading.rejected,
  .verdict.rejected {
    color: var(--color-log-warn);
  }

  .readout {
    margin: 0;
  }

  .row {
    display: grid;
    grid-template-columns: minmax(140px, 220px) 1fr;
    gap: var(--space-md);
    align-items: baseline;
    min-height: 28px;
    padding: var(--space-xs) 0;
  }

  .label {
    color: var(--color-log-info);
    font-weight: var(--weight-semibold);
  }

  .value {
    margin: 0;
    color: var(--color-body-text);
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .mono {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }

  .reason {
    margin: 0;
    color: var(--color-body-text);
    overflow-wrap: anywhere;
  }

  .fixed-note {
    margin: 0;
    color: var(--color-log-info);
    font-size: 12px;
  }
</style>
