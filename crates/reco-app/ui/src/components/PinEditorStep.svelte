<!--
  Pin step (MANU-03 / MANU-04): place correspondence pins on the paired source
  frames, see the warp preview update instantly under the current real
  parameters, and let the debounced background solve land with a visible
  "solving…" state. Pins can be dragged, deleted, and re-paired; each pin shows a
  verified/unverified state. When a partial calibration exists the editor is
  pre-populated with geometrically verified (post-RANSAC) matches only.

  All feedback drives real calibration parameters: the previews are the worker's
  GPU undistorts under real CameraParams, and the solve runs the engine's
  swap-correct manual helper. This component never derives a warp.
-->
<script lang="ts">
  import { manual } from "../lib/manual.svelte";
  import type { ManualPinView } from "../lib/types";
  import PinCanvas from "./PinCanvas.svelte";
  import ActionButton from "./ActionButton.svelte";
  import Icon from "./Icon.svelte";
  import InlineNotice from "./InlineNotice.svelte";

  let activeId = $state<number | null>(null);
  let pairing = $state(false);
  let pendingLeft = $state<[number, number] | null>(null);

  const pinCount = $derived(manual.pins.length);

  /** Worker-authoritative degeneracy: true only when the last completed solve
   *  was rejected because the pin set is coincident/collinear. Driven from the
   *  typed solve state, never inferred from `stale` (which also covers a
   *  debounce gap or a failed re-solve). */
  const degenerate = $derived(manual.degenerate);

  function rePair(pin: ManualPinView): void {
    // Re-pair: drop the old pin and restart the pair from its left point, so
    // the operator only needs to click the matching right-frame point.
    pendingLeft = pin.left_px;
    pairing = true;
    void manual.removePin(pin.id);
  }
</script>

<section class="pin-step" aria-label="Pin">
  <!-- The shared `SolveStatus` chip is flow chrome, rendered once in
       `ManualCalibrationFlow`'s header — not duplicated per step (UI-REVIEW). -->
  <div class="pin-head">
    <h3 class="step-heading">Pin</h3>
  </div>

  <p class="instruction" aria-live="polite">
    {#if pairing}
      Now click the matching point on the right frame.
    {:else}
      Click a point on the left frame, then its match on the right.
    {/if}
  </p>

  {#if manual.seeded}
    <InlineNotice
      level="info"
      message="Started from verified automatic matches — drag, delete, or add pins."
    />
  {/if}

  {#if degenerate}
    <InlineNotice
      level="warn"
      message="Pins are too close together to solve — spread them across the frames."
    />
  {/if}

  <PinCanvas bind:activeId bind:pairing bind:pendingLeft />

  {#if pinCount === 0}
    <p class="empty-hint">No pins yet. Click the left frame to start.</p>
  {:else}
    <p class="pin-count">{pinCount} pin{pinCount === 1 ? "" : "s"}</p>
    <ul class="pin-list" aria-label="Pins">
      {#each manual.pins as pin (pin.id)}
        <li class="pin-row" class:active={pin.id === activeId}>
          <span class="pin-label">Pin {pin.id + 1}</span>
          <span class="pin-state {pin.verified ? 'verified' : 'unverified'}">
            {pin.verified ? "verified" : "unverified"}
          </span>
          <span class="pin-spacer"></span>
          <button
            type="button"
            class="pin-action"
            onclick={() => rePair(pin)}
          >
            Re-pair
          </button>
          <button
            type="button"
            class="pin-action destructive"
            onclick={() => void manual.removePin(pin.id)}
          >
            Delete pin
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  <div class="pin-actions">
    <ActionButton
      variant="secondary"
      disabled={pinCount === 0}
      onClick={() => void manual.clearPins()}
    >
      <Icon name="trash" /> Clear pins
    </ActionButton>
    {#if manual.solveResult}
      <span class="residual-readout">
        residual {manual.solveResult.residual.toFixed(6)}
      </span>
    {/if}
  </div>
</section>

<style>
  .pin-step {
    display: flex;
    flex-direction: column;
    gap: var(--space-md);
  }

  .pin-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-md);
  }

  .step-heading {
    margin: 0;
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
    color: var(--color-body-text);
  }

  .instruction {
    margin: 0;
    color: var(--color-body-text);
  }

  .empty-hint {
    margin: 0;
    color: var(--color-log-info);
  }

  .pin-count {
    margin: 0;
    font-family: var(--font-mono);
    color: var(--color-log-info);
  }

  .pin-list {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .pin-row {
    display: flex;
    align-items: center;
    gap: var(--space-sm);
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-dominant);
  }

  .pin-row.active {
    border-color: var(--color-accent);
  }

  .pin-label {
    font-family: var(--font-mono);
    color: var(--color-body-text);
  }

  .pin-state {
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .pin-state.verified {
    color: var(--color-success);
  }

  .pin-state.unverified {
    color: var(--color-log-warn);
  }

  .pin-spacer {
    flex: 1 1 auto;
  }

  .pin-action {
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: transparent;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-label);
    cursor: pointer;
  }

  .pin-action.destructive {
    color: var(--color-log-error);
    border-color: var(--color-log-error);
  }

  .pin-action:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .pin-actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .residual-readout {
    font-family: var(--font-mono);
    color: var(--color-log-info);
  }
</style>
