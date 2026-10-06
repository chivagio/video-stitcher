<!--
  Validate step (MANU-07). Validates the manual result on at least one frame
  other than the calibration frame: a validation-frame selector, a Blink/Blend
  comparison of the stitched output, a monospace per-frame residual, and an
  advisory verdict. Validation is never a hard gate — Save stays available; the
  single-frame caveat is shown until an additional frame is validated. Save
  writes a normal calibration profile through the existing `.json` path.
-->
<script lang="ts">
  import { save } from "@tauri-apps/plugin-dialog";
  import { manual } from "../lib/manual.svelte";
  import { formatWorkerError } from "../lib/errors";
  import ValidationCanvas from "./ValidationCanvas.svelte";
  import ActionButton from "./ActionButton.svelte";
  import InlineNotice from "./InlineNotice.svelte";
  import Icon from "./Icon.svelte";

  /** The frame selected for validation (worker-clamped on validate). */
  let selectedFrame = $state(0);
  /** The comparison mode: Blink alternates, Blend composites 50/50. */
  let mode = $state<"blink" | "blend">("blink");
  /** Whether the selector has been defaulted for the current session. */
  let initialised = false;

  const framesTotal = $derived(manual.framesTotal);
  const maxFrame = $derived(framesTotal > 0 ? framesTotal - 1 : 0);
  const validation = $derived(manual.validationFrames[selectedFrame] ?? null);

  // Keep the selector inside the clip, and default it to a frame other than the
  // calibration frame (single-frame calibration must not be sold as truth).
  $effect(() => {
    const max = maxFrame;
    if (!initialised && manual.framesTotal > 0) {
      initialised = true;
      selectedFrame = manual.frame < max ? manual.frame + 1 : max;
    }
    if (selectedFrame > max) selectedFrame = max;
  });

  /** Whether an additional (non-calibration) frame has been validated. */
  const validatedAdditional = $derived(
    Object.keys(manual.validationFrames).some((k) => Number(k) !== manual.frame),
  );

  const residualText = $derived(
    validation !== null ? validation.residual.toFixed(4) : null,
  );
  const verdictText = $derived.by(() => {
    if (validation === null) return null;
    return validation.verdict === "looks_good" ? "Looks good" : "Check the seam";
  });

  function handleSelect(e: Event): void {
    selectedFrame = Number((e.target as HTMLInputElement).value);
  }

  function handleValidate(): void {
    void manual.validate(selectedFrame);
  }

  async function handleSave(): Promise<void> {
    try {
      const selected = await save({
        defaultPath: "manual-calibration.json",
        filters: [{ name: "Calibration profile", extensions: ["json"] }],
      });
      if (typeof selected !== "string") return;
      await manual.save(selected);
    } catch (e) {
      manual.error = formatWorkerError(e);
    }
  }
</script>

<section class="validate-step" aria-label="Validate">
  <h3 class="step-heading">Validate</h3>

  <div class="selector-row">
    <input
      type="range"
      class="frame-selector"
      min="0"
      max={maxFrame}
      step="1"
      value={selectedFrame}
      disabled={framesTotal === 0}
      aria-label="Validation frame"
      aria-valuetext="{selectedFrame + 1} of {framesTotal}"
      oninput={handleSelect}
      onchange={handleValidate}
    />
    <span class="frame-readout">
      Validation frame {selectedFrame + 1} of {framesTotal || 0}
    </span>
  </div>

  <div class="mode-row" role="group" aria-label="Comparison mode">
    <ActionButton
      variant={mode === "blink" ? "primary" : "secondary"}
      onClick={() => (mode = "blink")}
    >
      Blink
    </ActionButton>
    <ActionButton
      variant={mode === "blend" ? "primary" : "secondary"}
      onClick={() => (mode = "blend")}
    >
      Blend
    </ActionButton>
    <ActionButton
      variant="secondary"
      disabled={framesTotal === 0}
      onClick={handleValidate}
    >
      Validate frame
    </ActionButton>
  </div>

  <ValidationCanvas frame={selectedFrame} {mode} />

  {#if residualText !== null && validation !== null}
    <p class="residual-readout" aria-label="Per-frame residual">
      residual {residualText}
    </p>
    <p
      class="verdict"
      class:good={validation.verdict === "looks_good"}
      aria-label="Advisory verdict"
    >
      {verdictText}
    </p>
  {:else}
    <p class="empty-hint">
      Select a frame and validate it to compare the stitched output.
    </p>
  {/if}

  {#if !validatedAdditional}
    <InlineNotice
      level="warn"
      message="Calibrated on one frame — validate on another before saving."
    />
  {/if}

  {#if manual.savedPath !== null}
    <p class="save-line success">
      Calibration profile saved.
      <span class="mono">{manual.savedPath}</span>
    </p>
  {/if}

  <div class="validate-actions">
    <ActionButton variant="primary" onClick={() => void handleSave()}>
      <Icon name="save" /> Save calibration
    </ActionButton>
  </div>
</section>

<style>
  .validate-step {
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

  .selector-row {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }

  .frame-selector {
    flex: 1;
    height: 4px;
    -webkit-appearance: none;
    appearance: none;
    background: var(--color-dominant);
    border-radius: 2px;
    cursor: pointer;
    outline: none;
  }

  .frame-selector:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .frame-selector::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    cursor: pointer;
  }

  .frame-selector::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    border: none;
    cursor: pointer;
  }

  .frame-selector:focus-visible {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .frame-readout {
    font-family: var(--font-mono);
    color: var(--color-body-text);
    white-space: nowrap;
  }

  .mode-row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-sm);
  }

  .residual-readout {
    margin: 0;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-body-text);
  }

  .verdict {
    margin: 0;
    font-weight: var(--weight-semibold);
    color: var(--color-log-warn);
  }

  .verdict.good {
    color: var(--color-success);
  }

  .empty-hint {
    margin: 0;
    color: var(--color-log-info);
  }

  .save-line {
    margin: 0;
    font-size: var(--text-body);
    overflow-wrap: anywhere;
  }

  .save-line.success {
    color: var(--color-success);
  }

  .mono {
    font-family: var(--font-mono);
  }

  .validate-actions {
    display: flex;
    align-items: center;
    gap: var(--space-md);
  }
</style>
