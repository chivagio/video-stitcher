<!--
  Import screen (D3-01/02/03, IMPT-01..IMPT-06): a centered 960px column on an
  opaque dominant screen body. Two explicit camera slots side by side; below
  them the InlineNotice/CompatibilityBanner area; then the footer action row
  with the Calibrate CTA and the profile load/save actions.
-->
<script lang="ts">
  import type { InputRole } from "../lib/types";
  import { importStore } from "../lib/import.svelte";
  import FileDropSlot from "./FileDropSlot.svelte";
  import ActionButton from "./ActionButton.svelte";
  import CompatibilityBanner from "./CompatibilityBanner.svelte";
  import InlineNotice from "./InlineNotice.svelte";
  import ProfileActions from "./ProfileActions.svelte";

  let { onCalibrate }: { onCalibrate: () => void } = $props();

  const bothReady = $derived(importStore.bothReady);
  const bothEmpty = $derived(
    importStore.inputs.left.status === "empty" &&
      importStore.inputs.right.status === "empty",
  );
  const hasIssues = $derived(importStore.readiness.findings.length > 0);

  // "Review inputs" dismisses the banner so the operator can edit the slots.
  let bannerDismissed = $state(false);
  $effect(() => {
    // Reset the dismissal whenever the findings change.
    void importStore.readiness;
    bannerDismissed = false;
  });

  function handleChoose(role: InputRole): void {
    void importStore.chooseFile(role);
  }

  function handleDropFiles(role: InputRole, paths: string[]): void {
    void importStore.dropFiles(role, paths);
  }
</script>

<div class="import-screen">
  <div class="import-column">
    <header class="import-header">
      <h2 class="screen-title">Import clips</h2>
      {#if bothEmpty}
        <h3 class="empty-heading">No clips yet</h3>
        <p class="screen-subtitle">
          Drop two video files onto the slots, or choose one for each camera.
          Camera A is the left feed and Camera B is the right — the order matters
          for stitching.
        </p>
      {:else}
        <p class="screen-subtitle">
          Camera A is the left feed and Camera B is the right — the order matters
          for stitching.
        </p>
      {/if}
    </header>

    <div class="slots">
      <FileDropSlot
        slot={importStore.inputs.left}
        onChoose={handleChoose}
        onDropFiles={handleDropFiles}
      />
      <FileDropSlot
        slot={importStore.inputs.right}
        onChoose={handleChoose}
        onDropFiles={handleDropFiles}
      />
    </div>

    <div class="notices">
      {#if importStore.checksUnavailable}
        <InlineNotice
          level="warn"
          message={`Couldn't run compatibility checks: ${importStore.checksError ?? "unknown error"}. You can still calibrate.`}
        />
      {/if}
      {#if importStore.resultInvalidated}
        <InlineNotice
          level="warn"
          message="Inputs changed — the loaded calibration no longer matches. Re-run calibration."
        />
      {/if}
      {#if bothReady && hasIssues && !bannerDismissed}
        <CompatibilityBanner
          report={importStore.readiness}
          onCalibrateAnyway={onCalibrate}
          onReviewInputs={() => (bannerDismissed = true)}
        />
      {:else if bothReady && !hasIssues && !importStore.checksUnavailable}
        <p class="compatible-line">Inputs look compatible.</p>
      {/if}
    </div>

    <footer class="import-footer">
      <div class="footer-cta">
        <ActionButton
          variant="primary"
          disabled={!bothReady}
          title={bothReady ? "Start calibration" : "Select two clips to calibrate."}
          onClick={onCalibrate}
        >
          Calibrate
        </ActionButton>
        {#if !bothReady}
          <span class="disabled-hint">Select two clips to calibrate.</span>
        {/if}
      </div>
      <ProfileActions />
    </footer>
  </div>
</div>

<style>
  .import-screen {
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

  .import-column {
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

  .empty-heading {
    margin: var(--space-lg) 0 0;
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .screen-subtitle {
    margin: var(--space-sm) 0 0;
    color: var(--color-log-info);
    max-width: 60ch;
  }

  .slots {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-lg);
  }

  .notices {
    display: flex;
    flex-direction: column;
    gap: var(--space-sm);
  }

  .compatible-line {
    margin: 0;
    color: var(--color-success);
    font-size: var(--text-body);
  }

  .import-footer {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: var(--space-md);
  }

  .footer-cta {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .disabled-hint {
    color: var(--color-log-info);
    font-size: var(--text-body);
  }

  @media (max-width: 720px) {
    .slots {
      grid-template-columns: 1fr;
    }
  }
</style>
