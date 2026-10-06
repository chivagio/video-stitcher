<!--
  Advanced calibration disclosure (E12 / D3-12): collapsed by default; expanding
  reveals ONLY the four locked fields (Frames to sample / Skip start (s) / Skip
  end (s) / Use IMU rotation seeds). Raw AKAZE/match/optimizer thresholds stay
  hidden. Edited fields are tagged `modified`; invalid numeric input is rejected
  inline with the locked validation copy and blocks starting the run.

  The parent owns the emitted `CalibrationOptions` and the overall validity.
-->
<script lang="ts">
  import { untrack } from "svelte";
  import type { CalibrationOptions } from "../lib/types";
  import Icon from "./Icon.svelte";

  let {
    options,
    onChange,
  }: {
    options: CalibrationOptions;
    onChange: (options: CalibrationOptions, valid: boolean) => void;
  } = $props();

  /** Upper bound on sampled frame pairs (mirrors `validate_options`). */
  const FRAMES_MAX = 200;
  /** UI bound on the skip seconds (the engine only requires finite >= 0). */
  const SKIP_MAX = 3600;

  let expanded = $state(false);
  // Snapshot the initial prop values once: these are local drafts seeded from
  // the parent's options (the disclosure re-mounts when the screen changes).
  // `untrack` reads the prop without subscribing, which is the intent here.
  const initial = untrack(() => options);
  // Raw strings preserve an invalid entry so the inline error can be shown.
  let framesRaw = $state(
    initial.num_frames === null ? "" : String(initial.num_frames),
  );
  let skipStartRaw = $state(
    initial.skip_start_secs === null ? "" : String(initial.skip_start_secs),
  );
  let skipEndRaw = $state(
    initial.skip_end_secs === null ? "" : String(initial.skip_end_secs),
  );
  let imu = $state<boolean | null>(initial.use_imu_rotation_seeds);

  let frameError = $state<string | null>(null);
  let skipStartError = $state<string | null>(null);
  let skipEndError = $state<string | null>(null);

  function parseNumber(
    raw: string,
    min: number,
    max: number,
  ): { value: number | null; error: string | null } {
    const trimmed = raw.trim();
    if (trimmed === "") return { value: null, error: null };
    const n = Number(trimmed);
    if (!Number.isFinite(n) || n < min || n > max) {
      // The bounds are rendered from `min`/`max` (IN-04): frames are 1..max, so
      // a hardcoded "0" would claim an invalid value is acceptable.
      return { value: null, error: `Enter a value between ${min} and ${max}.` };
    }
    return { value: n, error: null };
  }

  function emit(): void {
    const frames = parseNumber(framesRaw, 1, FRAMES_MAX);
    const skipStart = parseNumber(skipStartRaw, 0, SKIP_MAX);
    const skipEnd = parseNumber(skipEndRaw, 0, SKIP_MAX);
    frameError = frames.error;
    skipStartError = skipStart.error;
    skipEndError = skipEnd.error;
    const valid =
      frames.error === null && skipStart.error === null && skipEnd.error === null;
    onChange(
      {
        num_frames: frames.value,
        skip_start_secs: skipStart.value,
        skip_end_secs: skipEnd.value,
        use_imu_rotation_seeds: imu,
      },
      valid,
    );
  }

  const framesModified = $derived(framesRaw.trim() !== "");
  const skipStartModified = $derived(skipStartRaw.trim() !== "");
  const skipEndModified = $derived(skipEndRaw.trim() !== "");
  const imuModified = $derived(imu !== null);
</script>

<section class="advanced">
  <button
    type="button"
    class="advanced-toggle"
    aria-expanded={expanded}
    onclick={() => (expanded = !expanded)}
  >
    <span class="chevron" class:open={expanded}><Icon name="chevron-down" /></span>
    Advanced
  </button>

  {#if expanded}
    <div class="advanced-body">
      <label class="field">
        <span class="field-label">
          Frames to sample
          {#if framesModified}<span class="modified">modified</span>{/if}
        </span>
        <input
          class="field-input"
          type="number"
          min="1"
          max={FRAMES_MAX}
          step="1"
          placeholder="Engine default"
          bind:value={framesRaw}
          oninput={emit}
        />
        {#if frameError}<span class="field-error">{frameError}</span>{/if}
      </label>

      <label class="field">
        <span class="field-label">
          Skip start (s)
          {#if skipStartModified}<span class="modified">modified</span>{/if}
        </span>
        <input
          class="field-input"
          type="number"
          min="0"
          max={SKIP_MAX}
          step="0.1"
          placeholder="Engine default"
          bind:value={skipStartRaw}
          oninput={emit}
        />
        {#if skipStartError}<span class="field-error">{skipStartError}</span>{/if}
      </label>

      <label class="field">
        <span class="field-label">
          Skip end (s)
          {#if skipEndModified}<span class="modified">modified</span>{/if}
        </span>
        <input
          class="field-input"
          type="number"
          min="0"
          max={SKIP_MAX}
          step="0.1"
          placeholder="Engine default"
          bind:value={skipEndRaw}
          oninput={emit}
        />
        {#if skipEndError}<span class="field-error">{skipEndError}</span>{/if}
      </label>

      <label class="field toggle">
        <input
          type="checkbox"
          checked={imu === true}
          onchange={(e) => {
            imu = e.currentTarget.checked;
            emit();
          }}
        />
        <span class="field-label">
          Use IMU rotation seeds
          {#if imuModified}<span class="modified">modified</span>{/if}
        </span>
      </label>
    </div>
  {/if}
</section>

<style>
  .advanced {
    border-top: 1px solid var(--color-secondary);
    padding-top: var(--space-sm);
  }

  .advanced-toggle {
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

  .chevron {
    display: inline-flex;
    transition: transform 150ms ease;
  }

  .chevron.open {
    transform: rotate(180deg);
  }

  .advanced-body {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: var(--space-md);
    margin-top: var(--space-sm);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    min-width: 0;
  }

  .field.toggle {
    flex-direction: row;
    align-items: center;
    gap: var(--space-sm);
  }

  .field-label {
    display: inline-flex;
    align-items: center;
    gap: var(--space-xs);
    color: var(--color-log-info);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
  }

  .field-input {
    min-height: 32px;
    padding: var(--space-xs) var(--space-sm);
    border: 1px solid var(--color-secondary);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-mono);
    font-size: var(--text-body);
  }

  .field-input:focus-visible {
    outline: none;
    border-color: var(--color-accent);
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .modified {
    padding: 0 var(--space-xs);
    border: 1px solid var(--color-log-warn);
    border-radius: var(--space-xs);
    color: var(--color-log-warn);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
  }

  .field-error {
    color: var(--color-log-error);
    font-size: var(--text-body);
  }

  @media (prefers-reduced-motion: reduce) {
    .chevron {
      transition: none;
    }
  }

  @media (max-width: 720px) {
    .advanced-body {
      grid-template-columns: 1fr;
    }
  }
</style>
