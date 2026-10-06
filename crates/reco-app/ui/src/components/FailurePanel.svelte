<!--
  Failure panel (CALB-04): the honest, plain-language explanation of a failed
  calibration. The cause and fix are authored in Rust on the typed
  `CalibrationDiagnosis` DTO; this component renders them verbatim and never
  invents a cause or shows a bare error code. The raw typed error and the
  aggregated stage metrics sit behind a collapsed `Technical detail` disclosure.
-->
<script lang="ts">
  import type { CalibrationDiagnosis } from "../lib/types";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";

  let {
    diagnosis,
    error = null,
    onTryAgain,
    onBackToImport,
    onCalibrateManually,
  }: {
    diagnosis: CalibrationDiagnosis | null;
    error?: string | null;
    onTryAgain: () => void;
    onBackToImport: () => void;
    onCalibrateManually: () => void;
  } = $props();

  // Report honestly when the run failed before any frame pair produced matches:
  // show a "no stage metrics" line rather than a row of fabricated zeros
  // (UI-SPEC Failure Diagnosis Contract — partial state).
  const hasMetrics = $derived(
    diagnosis !== null &&
      (diagnosis.metrics.frames_used > 0 || diagnosis.metrics.total_matches > 0),
  );
</script>

<section class="failure-panel" aria-label="Calibration failure">
  {#if diagnosis !== null}
    <div class="block">
      <h3 class="block-label cause-label">
        <Icon name="alert-triangle" />
        What happened
      </h3>
      <p class="cause">{diagnosis.cause}</p>
    </div>

    <div class="block">
      <h3 class="block-label fix-label">
        <Icon name="wrench" />
        What to try
      </h3>
      <p class="fix">{diagnosis.fix}</p>
    </div>

    <details class="technical">
      <summary>Technical detail</summary>
      <div class="technical-body">
        <p class="raw-error">{diagnosis.raw_error}</p>
        {#if hasMetrics}
          <p class="metrics">
            Frames used {diagnosis.metrics.frames_used} · matches {diagnosis.metrics.total_matches} ·
            after ratio {diagnosis.metrics.post_ratio_test} · after spatial {diagnosis.metrics
              .post_spatial_filter} · after RANSAC {diagnosis.metrics.post_ransac} · keypoints L/R {diagnosis
              .metrics.keypoints_left}/{diagnosis.metrics.keypoints_right}
          </p>
        {:else}
          <p class="metrics-none">No stage metrics were collected for this run.</p>
        {/if}
      </div>
    </details>
  {:else}
    <div class="block">
      <h3 class="block-label cause-label">
        <Icon name="alert-triangle" />
        What happened
      </h3>
      <p class="cause">
        {error ?? "Calibration failed, but no diagnosis was reported."}
      </p>
    </div>
    <div class="block">
      <h3 class="block-label fix-label">
        <Icon name="wrench" />
        What to try
      </h3>
      <p class="fix">Try again, or go back to Import to change the clips.</p>
    </div>
  {/if}

  <div class="actions">
    <ActionButton variant="primary" onClick={onTryAgain}>Try again</ActionButton>
    <ActionButton variant="secondary" onClick={onCalibrateManually}>
      Calibrate manually
    </ActionButton>
    <ActionButton variant="secondary" onClick={onBackToImport}>
      Back to import
    </ActionButton>
  </div>
</section>

<style>
  .failure-panel {
    padding: var(--space-md);
    border: 1px solid var(--color-log-error);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
    overflow-wrap: anywhere;
  }

  .block {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .block-label {
    margin: 0;
    display: flex;
    align-items: center;
    gap: var(--space-xs);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .cause-label {
    color: var(--color-log-error);
  }

  .fix-label {
    color: var(--color-log-info);
  }

  .cause,
  .fix {
    margin: 0;
    color: var(--color-body-text);
    max-width: 72ch;
  }

  .technical {
    border-top: 1px solid var(--color-dominant);
    padding-top: var(--space-sm);
  }

  .technical summary {
    cursor: pointer;
    color: var(--color-log-info);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .technical-body {
    margin-top: var(--space-sm);
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .raw-error,
  .metrics,
  .metrics-none {
    margin: 0;
    font-family: var(--font-mono);
    font-size: var(--text-body);
    color: var(--color-log-info);
    overflow-wrap: anywhere;
  }

  .metrics-none {
    color: var(--color-log-info);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }
</style>
