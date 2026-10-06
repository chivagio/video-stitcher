<!--
  Calibrate screen (E9 / CALB-01/02/03): a single screen with ready / running /
  result / failed states. The checklist, progress and heartbeat are rendered
  from the typed worker events; the screen never derives stage state locally.
  Cancel is confirmed by the App-owned ConfirmDialog before `cancel()`.

  Ready: title + Start CTA + the seven pending stages + a 0% progress bar.
  Running: the live checklist + progress + heartbeat + Cancel CTA.
  Failed: the typed error + Try again / Back to import (no partial scorecard).
  Result: the CALB-03 scorecard (Task 2).
-->
<script lang="ts">
  import { calibration } from "../lib/calibration.svelte";
  import ActionButton from "./ActionButton.svelte";
  import StageChecklist from "./StageChecklist.svelte";
  import CalibrationProgress from "./CalibrationProgress.svelte";

  let {
    onRequestCancel,
    onBackToImport,
  }: {
    onRequestCancel: () => void;
    onBackToImport: () => void;
  } = $props();

  function handleStart(): void {
    void calibration.start({
      num_frames: null,
      skip_start_secs: null,
      skip_end_secs: null,
      use_imu_rotation_seeds: null,
    });
  }
</script>

<div class="calibrate-screen">
  <div class="calibrate-column">
    {#if calibration.status === "done" && calibration.result !== null}
      <header class="screen-header">
        <h2 class="screen-title">Calibration result</h2>
      </header>
      <!-- Task 2 replaces this with the CALB-03 scorecard. -->
      <p class="result-placeholder">Calibration complete.</p>
    {:else if calibration.status === "failed"}
      <header class="screen-header">
        <h2 class="screen-title">Calibration failed</h2>
      </header>
      <p class="failure-body">
        {calibration.error ?? "Unknown error"}. Try again, or go back to Import to
        change the clips.
      </p>
      <div class="actions">
        <ActionButton variant="primary" onClick={handleStart}>
          Try again
        </ActionButton>
        <ActionButton variant="secondary" onClick={onBackToImport}>
          Back to import
        </ActionButton>
      </div>
    {:else}
      <header class="screen-header">
        <h2 class="screen-title">
          {calibration.status === "ready" ? "Ready to calibrate" : "Calibrating…"}
        </h2>
        {#if calibration.status === "ready"}
          <p class="screen-subtitle">
            Both clips are loaded and checked. Start calibration when you're
            ready.
          </p>
        {/if}
      </header>

      <StageChecklist />
      <CalibrationProgress />

      <div class="actions">
        {#if calibration.status === "ready"}
          <ActionButton variant="primary" onClick={handleStart}>
            Start calibration
          </ActionButton>
        {:else}
          <ActionButton
            variant="destructive"
            disabled={calibration.status === "cancelling"}
            onClick={onRequestCancel}
          >
            {calibration.status === "cancelling" ? "Cancelling…" : "Cancel calibration"}
          </ActionButton>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .calibrate-screen {
    position: fixed;
    top: var(--workflow-rail-height);
    left: 0;
    right: 0;
    bottom: 0;
    /* Opaque dominant: no native preview surface may show through (UI-SPEC
       Screen Router). The native view is suspended on this screen by Rust. */
    background: var(--color-dominant);
    overflow-y: auto;
  }

  .calibrate-column {
    max-width: 960px;
    margin: 0 auto;
    padding: var(--space-3xl) var(--space-lg);
    display: flex;
    flex-direction: column;
    gap: var(--space-xl);
  }

  .screen-title {
    margin: 0;
    font-size: var(--text-display);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .screen-subtitle {
    margin: var(--space-sm) 0 0;
    color: var(--color-log-info);
    max-width: 60ch;
  }

  .failure-body {
    margin: 0;
    color: var(--color-log-error);
    overflow-wrap: anywhere;
    max-width: 72ch;
  }

  .result-placeholder {
    margin: 0;
    color: var(--color-log-info);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }
</style>
