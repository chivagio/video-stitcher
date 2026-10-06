<!--
  Manual calibration flow shell (MANU-01 / UI-SPEC Manual Flow Contract).
  A webview-owned full-screen surface hosted inside the Calibrate screen
  (native child view suspended), so pointer input and canvas compositing work.
  It owns the 5-step sub-nav with free back-navigation, the shared solve-status
  chip, and the active step body. The flow is always exitable without losing an
  existing profile.
-->
<script lang="ts">
  import {
    manual,
    MANUAL_STEPS,
    MANUAL_STEP_NAMES,
  } from "../lib/manual.svelte";
  import FramePickStep from "./FramePickStep.svelte";
  import SolveStatus from "./SolveStatus.svelte";
  import ActionButton from "./ActionButton.svelte";
  import InlineNotice from "./InlineNotice.svelte";

  let { onExit }: { onExit: () => void } = $props();

  function handleExit(): void {
    void manual.exit();
    onExit();
  }
</script>

<section class="manual-flow" aria-label="Manual calibration">
  <header class="flow-header">
    <h2 class="flow-title">Manual calibration</h2>
    <SolveStatus />
  </header>

  {#if manual.error !== null}
    <InlineNotice level="error" message={manual.error} />
  {/if}

  <nav class="step-nav" aria-label="Manual calibration steps">
    <ol class="step-list">
      {#each MANUAL_STEPS as step, i (step)}
        <li class="step-item">
          <button
            type="button"
            class="step-btn"
            class:current={manual.step === step}
            class:visited={manual.stepIndex > i}
            class:future={manual.stepIndex < i}
            aria-current={manual.step === step ? "step" : undefined}
            onclick={() => manual.goToStep(step)}
          >
            <span class="step-index">{i + 1}</span>
            <span class="step-name">{MANUAL_STEP_NAMES[step]}</span>
          </button>
        </li>
      {/each}
    </ol>
  </nav>

  <div class="step-body">
    {#if manual.step === "frame"}
      <FramePickStep />
    {:else}
      <div class="step-placeholder">
        <p class="placeholder-text">
          {MANUAL_STEP_NAMES[manual.step]} — not yet configured.
        </p>
      </div>
    {/if}
  </div>

  <div class="flow-actions">
    <ActionButton
      variant="secondary"
      disabled={manual.stepIndex === 0}
      onClick={() => manual.back()}
    >
      Back
    </ActionButton>
    <ActionButton
      variant="primary"
      disabled={manual.stepIndex === MANUAL_STEPS.length - 1}
      onClick={() => manual.next()}
    >
      Next
    </ActionButton>
    <span class="spacer"></span>
    <ActionButton variant="destructive" onClick={handleExit}>
      Exit manual calibration
    </ActionButton>
  </div>
</section>

<style>
  .manual-flow {
    max-width: 960px;
    margin: 0 auto;
    padding: var(--space-3xl) var(--space-lg);
    display: flex;
    flex-direction: column;
    gap: var(--space-lg);
  }

  .flow-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-md);
  }

  .flow-title {
    margin: 0;
    font-size: var(--text-display);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  .step-list {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-sm);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .step-btn {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-log-info);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    cursor: pointer;
  }

  .step-btn.current {
    color: var(--color-accent);
    border-color: var(--color-accent);
  }

  .step-btn.visited {
    color: var(--color-body-text);
  }

  .step-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .step-index {
    font-family: var(--font-mono);
    opacity: 0.8;
  }

  .step-body {
    min-height: 240px;
    padding: var(--space-md);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
  }

  .step-placeholder {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 200px;
  }

  .placeholder-text {
    margin: 0;
    color: var(--color-log-info);
  }

  .flow-actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .spacer {
    flex: 1 1 auto;
  }
</style>
