<!--
  Time-align step (MANU-02 / UI-SPEC Step 1 locked copy).
  Audio auto-sync runs on entry and its confidence is shown; the operator
  corrects it with a fine frame nudge whose readout always states the engine's
  offset semantics in plain words. The semantics sentence is bound from the
  store's `offsetSemantics` (sourced from the single Rust `SYNC_OFFSET_SEMANTICS`
  constant) and is never re-typed here (T-04.1-07). When the estimate is absent
  or low the same "set the offset manually" gate applies and the offset must be
  explicitly confirmed before proceeding.
-->
<script lang="ts">
  import { onMount } from "svelte";
  import { manual } from "../lib/manual.svelte";
  import ActionButton from "./ActionButton.svelte";
  import InlineNotice from "./InlineNotice.svelte";

  // Audio auto-sync runs on entry to the step (MANU-02).
  onMount(() => {
    void manual.detectSync();
  });

  /** The confidence as a whole percent, or null when unavailable. */
  const confidencePct = $derived(
    manual.audioConfidence === null
      ? null
      : Math.round(manual.audioConfidence * 100),
  );

  /** The signed offset text — the sign is always shown (`+0`, `−7`). */
  const offsetText = $derived(
    `${manual.syncOffset < 0 ? "−" : "+"}${Math.abs(manual.syncOffset)}`,
  );

  /** Apply a fine frame nudge and record it as a manual offset. */
  function nudge(delta: number): void {
    void manual.setSync(manual.syncOffset + delta);
  }
</script>

<section class="time-align-step" aria-label="Time alignment">
  <h3 class="step-heading">Audio sync</h3>

  {#if confidencePct !== null}
    <p class="confidence-readout">Audio · {confidencePct}%</p>
  {/if}

  {#if manual.syncGateRequired}
    <InlineNotice
      level="warn"
      message="Audio sync unavailable or low confidence — set the offset manually."
    />
  {/if}

  <div class="nudge">
    <span class="nudge-label" id="time-align-nudge-label">Nudge offset</span>
    <div
      class="nudge-controls"
      role="group"
      aria-labelledby="time-align-nudge-label"
    >
      <button
        type="button"
        class="nudge-btn"
        aria-label="Skip one frame earlier"
        onclick={() => nudge(-1)}
      >
        −
      </button>
      <span
        class="offset-readout"
        id="time-align-offset"
        aria-describedby="time-align-semantics"
      >
        offset {offsetText} frames
      </span>
      <button
        type="button"
        class="nudge-btn"
        aria-label="Skip one frame later"
        onclick={() => nudge(1)}
      >
        +
      </button>
    </div>
    <p class="semantics" id="time-align-semantics">{manual.offsetSemantics}</p>
  </div>

  <div class="time-align-actions">
    <ActionButton variant="secondary" onClick={() => manual.confirmSync()}>
      Confirm offset
    </ActionButton>
    {#if manual.syncConfirmed}
      <span class="confirmed-badge" role="status">Confirmed</span>
    {/if}
  </div>
</section>

<style>
  .time-align-step {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .step-heading {
    margin: 0;
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .confidence-readout {
    margin: 0;
    font-family: var(--font-mono);
    color: var(--color-body-text);
  }

  .nudge {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .nudge-label {
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .nudge-controls {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
  }

  .nudge-btn {
    min-width: 36px;
    min-height: 36px;
    padding: 0 var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-mono);
    font-size: var(--text-body);
    cursor: pointer;
  }

  .nudge-btn:hover {
    filter: brightness(1.1);
  }

  .nudge-btn:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .offset-readout {
    font-family: var(--font-mono);
    color: var(--color-body-text);
    white-space: nowrap;
  }

  .semantics {
    margin: 0;
    color: var(--color-log-info);
    font-size: var(--text-label);
  }

  .time-align-actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .confirmed-badge {
    font-family: var(--font-mono);
    font-size: var(--text-label);
    color: var(--color-log-info);
  }
</style>
