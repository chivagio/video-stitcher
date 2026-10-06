<!--
  Overall calibration progress (E11 / CALB-01): a determinate progress bar +
  "Step n of 7 — <stage>" + "Elapsed M:SS" + a heartbeat line so a silent stage
  never looks stuck. The bar never blanks or spins; on failure it stops and
  turns error-coloured.

  Accessibility: the progress bar carries the progressbar ARIA contract; the
  "Step n of 7 — <stage>" label is the `role="status"` region and changes only
  on a stage transition (never on the ~500 ms heartbeat tick), so a screen
  reader is not spammed. The heartbeat's "last update M:SS ago" is visual-only.
-->
<script lang="ts">
  import { calibration, STAGE_NAMES, formatMSS } from "../lib/calibration.svelte";

  const pct = $derived(
    Math.round(Math.min(1, Math.max(0, calibration.fraction)) * 100),
  );
  const stageName = $derived(STAGE_NAMES[calibration.currentStage]);
  const elapsed = $derived(formatMSS(calibration.elapsedMs));
  const ago = $derived(formatMSS(calibration.heartbeatAgoMs));
  const failed = $derived(calibration.status === "failed");
</script>

<div class="progress-block">
  <div
    class="progress-bar"
    class:failed
    role="progressbar"
    aria-valuemin="0"
    aria-valuemax="100"
    aria-valuenow={pct}
    aria-valuetext={`Step ${calibration.currentIndex} of 7, ${stageName}`}
  >
    <div class="fill" style={`width: ${pct}%`}></div>
  </div>

  <p class="progress-label" role="status">
    Step {calibration.currentIndex} of 7 — {stageName}
  </p>

  <p class="elapsed">Elapsed {elapsed}</p>

  {#if calibration.status === "running" || calibration.status === "cancelling"}
    <p class="heartbeat" aria-hidden="true">
      Still working — {stageName} · last update {ago} ago
    </p>
  {/if}
</div>

<style>
  .progress-block {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .progress-bar {
    height: 8px;
    border-radius: 4px;
    background: var(--color-dominant);
    overflow: hidden;
  }

  .fill {
    height: 100%;
    background: var(--color-accent);
    transition: width 250ms ease;
  }

  .progress-bar.failed .fill {
    background: var(--color-log-error);
  }

  .progress-label {
    margin: 0;
    color: var(--color-body-text);
    font-weight: var(--weight-semibold);
  }

  .elapsed,
  .heartbeat {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .elapsed {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }

  .heartbeat {
    overflow-wrap: anywhere;
  }

  @media (prefers-reduced-motion: reduce) {
    .fill {
      transition: none;
    }
  }
</style>
