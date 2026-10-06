<!--
  Calibrate screen (E9 / CALB-01/02/03): a single screen with ready / running /
  result / failed states. The checklist, progress and heartbeat are rendered
  from the typed worker events; the screen never derives stage state locally.
  Cancel is confirmed by the App-owned ConfirmDialog before `cancel()`.

  Ready: title + Start CTA + the seven pending stages + a 0% progress bar.
  Running: the live checklist + progress + heartbeat + Cancel CTA.
  Failed: the typed error + Try again / Back to import (no partial scorecard).
  Result: the CALB-03 scorecard with Save profile… / Re-run calibration.
-->
<script lang="ts">
  import type { CalibrationOptions } from "../lib/types";
  import { calibration, defaultOptions } from "../lib/calibration.svelte";
  import ActionButton from "./ActionButton.svelte";
  import StageChecklist from "./StageChecklist.svelte";
  import CalibrationProgress from "./CalibrationProgress.svelte";
  import AdvancedDisclosure from "./AdvancedDisclosure.svelte";
  import Scorecard from "./Scorecard.svelte";
  import FailurePanel from "./FailurePanel.svelte";
  import DebugSection from "./DebugSection.svelte";
  import FieldRoiEditor from "./FieldRoiEditor.svelte";
  import CompatibilityBanner from "./CompatibilityBanner.svelte";
  import ManualCalibrationFlow from "./ManualCalibrationFlow.svelte";
  import Icon from "./Icon.svelte";
  import { importStore } from "../lib/import.svelte";
  import { manual } from "../lib/manual.svelte";

  let {
    onRequestCancel,
    onBackToImport,
  }: {
    onRequestCancel: () => void;
    onBackToImport: () => void;
  } = $props();

  // The advanced options are owned here so both the ready-state disclosure and
  // the result-state "Re-run calibration" use the same values.
  let options = $state<CalibrationOptions>(defaultOptions());
  let advancedValid = $state(true);

  // The Field ROI editor (CALB-09) is a collapsible section, reachable from the
  // ready and result states. It renders its own (webview-owned) canvas, so it is
  // fully operable on this screen regardless of the presenter.
  let fieldRoiOpen = $state(false);

  function handleStart(): void {
    if (!advancedValid) return;
    void calibration.start(options);
  }

  function handleRerun(): void {
    if (!advancedValid) return;
    void calibration.start(options);
  }

  function handleOptions(next: CalibrationOptions, valid: boolean): void {
    options = next;
    advancedValid = valid;
  }

  // Open the manual calibration flow (MANU-01). Always reachable from the ready
  // state and the failure panel; no calibration `.json` is ever required.
  function handleManualStart(): void {
    void manual.begin(0);
  }
</script>

<div class="calibrate-screen">
  {#if manual.open}
    <ManualCalibrationFlow onExit={() => {}} />
  {:else}
  <div class="calibrate-column">
    {#if calibration.status === "done" && calibration.result !== null}
      <header class="screen-header">
        <h2 class="screen-title">Calibration result</h2>
      </header>
      <Scorecard scorecard={calibration.result} onRerun={handleRerun} />
      <section class="roi-section">
        <button
          type="button"
          class="roi-toggle"
          aria-expanded={fieldRoiOpen}
          onclick={() => (fieldRoiOpen = !fieldRoiOpen)}
        >
          <span class="chevron" class:open={fieldRoiOpen}>
            <Icon name="chevron-down" />
          </span>
          <Icon name="polygon" />
          Field ROI
        </button>
        {#if fieldRoiOpen}
          <FieldRoiEditor report={calibration.debug} />
        {/if}
      </section>
      <DebugSection report={calibration.debug} ran={true} />
    {:else if calibration.status === "failed"}
      <header class="screen-header">
        <h2 class="screen-title">Calibration failed</h2>
      </header>
      <FailurePanel
        diagnosis={calibration.diagnosis}
        error={calibration.error}
        onTryAgain={handleStart}
        onBackToImport={onBackToImport}
        onCalibrateManually={handleManualStart}
      />
      <DebugSection report={calibration.debug} ran={true} />
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

      {#if calibration.status === "ready" && importStore.readiness.findings.length > 0}
        <CompatibilityBanner
          report={importStore.readiness}
          onCalibrateAnyway={handleStart}
          onReviewInputs={onBackToImport}
        />
      {/if}

      <StageChecklist />
      <CalibrationProgress />

      {#if calibration.status === "ready"}
        <AdvancedDisclosure {options} onChange={handleOptions} />
      {/if}

      {#if calibration.status === "ready"}
        <section class="roi-section">
          <button
            type="button"
            class="roi-toggle"
            aria-expanded={fieldRoiOpen}
            onclick={() => (fieldRoiOpen = !fieldRoiOpen)}
          >
            <span class="chevron" class:open={fieldRoiOpen}>
              <Icon name="chevron-down" />
            </span>
            <Icon name="polygon" />
            Field ROI
          </button>
          {#if fieldRoiOpen}
            <FieldRoiEditor report={calibration.debug} canSave={importStore.hasResult} />
          {/if}
        </section>
      {/if}

      <div class="actions">
        {#if calibration.status === "ready"}
          <ActionButton
            variant="primary"
            disabled={!advancedValid}
            onClick={handleStart}
          >
            Start calibration
          </ActionButton>
          <ActionButton variant="secondary" onClick={handleManualStart}>
            Calibrate manually
          </ActionButton>
          {#if !advancedValid}
            <span class="disabled-hint">
              Fix the advanced values before starting.
            </span>
          {/if}
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
  {/if}
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

  .disabled-hint {
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .roi-section {
    border-top: 1px solid var(--color-secondary);
    padding-top: var(--space-sm);
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .roi-toggle {
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

  .roi-toggle:focus-visible {
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

  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
  }
</style>
