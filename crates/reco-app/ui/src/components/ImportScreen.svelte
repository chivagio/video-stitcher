<!--
  Import screen (D3-01/02/03, IMPT-01/02): a centered 960px column on an opaque
  dominant screen body. Two explicit camera slots side by side; a footer CTA
  enabled only when both clips are valid.
-->
<script lang="ts">
  import type { InputRole } from "../lib/types";
  import { importStore } from "../lib/import.svelte";
  import FileDropSlot from "./FileDropSlot.svelte";
  import ActionButton from "./ActionButton.svelte";

  let { onCalibrate }: { onCalibrate: () => void } = $props();

  const bothReady = $derived(
    importStore.inputs.left.status === "ready" &&
      importStore.inputs.right.status === "ready",
  );

  function handleChoose(role: InputRole): void {
    void importStore.chooseFile(role);
  }

  function handleDrop(role: InputRole, path: string): void {
    void importStore.setPathFromDrop(role, path);
  }
</script>

<div class="import-screen">
  <div class="import-column">
    <header class="import-header">
      <h2 class="screen-title">Import clips</h2>
      <p class="screen-subtitle">
        Camera A is the left feed and Camera B is the right — the order matters
        for stitching.
      </p>
    </header>

    <div class="slots">
      <FileDropSlot
        slot={importStore.inputs.left}
        onChoose={handleChoose}
        onDrop={handleDrop}
      />
      <FileDropSlot
        slot={importStore.inputs.right}
        onChoose={handleChoose}
        onDrop={handleDrop}
      />
    </div>

    <footer class="import-footer">
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
    </footer>
  </div>
</div>

<style>
  .import-screen {
    position: fixed;
    top: 48px;
    left: 0;
    right: 0;
    bottom: 0;
    /* Opaque dominant: no native preview surface may show through (UI-SPEC
       Screen Router). The native-view suspension itself is plan 03-04. */
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
    font-size: 28px;
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .screen-subtitle {
    margin: var(--space-sm) 0 0;
    color: var(--color-log-info);
  }

  .slots {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-lg);
  }

  .import-footer {
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
