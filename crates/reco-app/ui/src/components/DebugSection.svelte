<!--
  Debug inspector section (CALB-08 / E4). A collapsible disclosure, collapsed by
  default, that hosts the feature-match overlay, the residual map, and the
  per-frame match-count table for the run's sampled frame pair. It is available
  after a run finishes (pass or fail) and scrolls within the Calibrate screen.
  The debug payload is delivered over the typed worker event, never a CLI PNG.
-->
<script lang="ts">
  import type { DebugReport } from "../lib/types";
  import Icon from "./Icon.svelte";
  import MatchOverlay from "./MatchOverlay.svelte";
  import ResidualMap from "./ResidualMap.svelte";
  import MatchCountTable from "./MatchCountTable.svelte";

  let {
    report = null,
    ran = false,
  }: {
    report?: DebugReport | null;
    /** Whether a calibration run has finished (pass or fail). */
    ran?: boolean;
  } = $props();

  let expanded = $state(false);
  /** The frame highlighted by the selector; defaults to the sampled frame. */
  let selectedFrame = $state(0);

  // Keep the selector on the sampled frame whenever a new report arrives.
  $effect(() => {
    if (report !== null) selectedFrame = report.frame_index;
  });

  // A report is "has data" when it carries per-frame rows or any points. An
  // empty report (a run that failed before matching) is shown as no-data.
  const hasData = $derived(
    report !== null &&
      (report.per_frame.length > 0 ||
        report.verified.length > 0 ||
        report.rejected.length > 0),
  );
</script>

<section class="debug-section">
  <button
    type="button"
    class="debug-toggle"
    aria-expanded={expanded}
    onclick={() => (expanded = !expanded)}
  >
    <span class="chevron" class:open={expanded}>
      <Icon name="chevron-down" />
    </span>
    <Icon name="bug" />
    Debug
  </button>

  {#if expanded}
    <div class="debug-body">
      {#if report !== null && hasData}
        {#if report.frames_total > 1}
          <label class="frame-select">
            Frame
            <select bind:value={selectedFrame} aria-label="Frame selector">
              {#each report.per_frame as row (row.frame)}
                <option value={row.frame}>
                  {row.frame + 1} of {report.frames_total}
                </option>
              {/each}
            </select>
          </label>
        {:else}
          <p class="frame-label">
            Frame {report.frame_index + 1} of {report.frames_total}
          </p>
        {/if}

        <MatchOverlay {report} />
        <ResidualMap {report} />
        <MatchCountTable rows={report.per_frame} />
      {:else if ran}
        <p class="empty">No match data for this run.</p>
      {:else}
        <p class="empty">Run a calibration to inspect matches.</p>
      {/if}
    </div>
  {/if}
</section>

<style>
  .debug-section {
    border-top: 1px solid var(--color-secondary);
    padding-top: var(--space-sm);
  }

  .debug-toggle {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    border: none;
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .debug-toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .chevron {
    display: inline-flex;
    transition: transform 150ms ease;
  }

  .chevron.open {
    transform: rotate(180deg);
  }

  .debug-body {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
    margin-top: var(--space-sm);
    /* The section scrolls within the screen rather than overflowing it. */
    max-width: 100%;
    min-width: 0;
  }

  .frame-select {
    display: inline-flex;
    align-items: center;
    gap: var(--space-sm);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    color: var(--color-log-info);
  }

  .frame-select select {
    min-height: 32px;
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-mono);
    font-size: var(--text-body);
  }

  .frame-select select:focus-visible {
    outline: none;
    border-color: var(--color-accent);
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .frame-label {
    margin: 0;
    font-family: var(--font-mono);
    font-size: var(--text-body);
    color: var(--color-log-info);
  }

  .empty {
    margin: 0;
    color: var(--color-log-info);
  }

  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
  }
</style>
