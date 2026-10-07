<!--
  Trim in/out handles (EXPT-03). Two keyboard-operable handles over the timeline
  track, a monospace `<in> → <out>` timecode readout, and a "Full clip" reset.

  The handles are native `<input type="range">` elements (arrow-key operable),
  each with an `aria-label` and an `aria-valuetext` timecode. The window is
  clamped so an out <= in can never be emitted (an empty export is impossible);
  a clamp is announced via `InlineNotice`, never silent. A handle at the clip
  edge emits `null` for that side, which is the "no trim here" representation the
  worker's `ExportSettings` uses.
-->
<script lang="ts">
  import ActionButton from "./ActionButton.svelte";
  import InlineNotice from "./InlineNotice.svelte";

  let {
    total,
    inFrame,
    outFrame,
    format,
    onChange,
  }: {
    /** Total frames in the source (the track extent); always > 1 here. */
    total: number;
    /** The in point, or `null` for the clip start. */
    inFrame: number | null;
    /** The out point, or `null` for the clip end. */
    outFrame: number | null;
    /** Format a frame index as a timecode (the transport's exact formatter). */
    format: (frame: number) => string;
    /** Emit the new (in, out) pair; `null` means "the clip edge". */
    onChange: (inFrame: number | null, outFrame: number | null) => void;
  } = $props();

  const max = $derived(total > 0 ? total - 1 : 0);
  const inValue = $derived(inFrame ?? 0);
  const outValue = $derived(outFrame ?? max);

  let clamped = $state(false);

  /** Normalize a clip-edge value to `null` (the "no trim" representation). */
  function emit(safeIn: number, safeOut: number): void {
    onChange(safeIn <= 0 ? null : safeIn, safeOut >= max ? null : safeOut);
  }

  function handleIn(e: Event): void {
    const requested = Number((e.currentTarget as HTMLInputElement).value);
    // The out point must stay strictly after the in point.
    const safeIn = Math.max(0, Math.min(requested, outValue - 1));
    clamped = safeIn !== requested;
    emit(safeIn, outValue);
  }

  function handleOut(e: Event): void {
    const requested = Number((e.currentTarget as HTMLInputElement).value);
    const safeOut = Math.min(max, Math.max(requested, inValue + 1));
    clamped = safeOut !== requested;
    emit(inValue, safeOut);
  }

  function resetFullClip(): void {
    clamped = false;
    onChange(null, null);
  }
</script>

<div class="trim">
  <div class="trim-track" aria-hidden="true">
    <div
      class="trim-range"
      style="left: {max > 0 ? (inValue / max) * 100 : 0}%; right: {max > 0 ? 100 - (outValue / max) * 100 : 0}%"
    ></div>
  </div>
  <div class="trim-handles">
    <input
      type="range"
      class="handle handle-in"
      min="0"
      {max}
      step="1"
      value={inValue}
      aria-label="Trim in"
      aria-valuetext={format(inValue)}
      oninput={handleIn}
    />
    <input
      type="range"
      class="handle handle-out"
      min="0"
      {max}
      step="1"
      value={outValue}
      aria-label="Trim out"
      aria-valuetext={format(outValue)}
      oninput={handleOut}
    />
  </div>
  <div class="trim-readout">
    <output class="trim-time" aria-label="Trim range">
      {format(inValue)} → {format(outValue)}
    </output>
    <ActionButton variant="secondary" ariaLabel="Reset trim to the full clip" onClick={resetFullClip}>
      Full clip
    </ActionButton>
  </div>
  {#if clamped}
    <InlineNotice level="warn" message="Trim out must be after trim in — clamped to the nearest frame." />
  {/if}
</div>

<style>
  .trim {
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
    min-width: 0;
  }

  .trim-track {
    position: relative;
    height: 4px;
    border-radius: 2px;
    background: var(--color-dominant);
  }

  .trim-range {
    position: absolute;
    top: 0;
    bottom: 0;
    background: var(--color-accent);
    border-radius: 2px;
    opacity: 0.5;
  }

  .trim-handles {
    position: relative;
    height: 16px;
    margin-top: -10px;
  }

  .handle {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 16px;
    margin: 0;
    -webkit-appearance: none;
    appearance: none;
    background: transparent;
    /* The track is painted by `.trim-track`; only the thumbs take pointer
       events so both handles stay draggable in the overlap. */
    pointer-events: none;
  }

  .handle::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 2px;
    background: var(--color-accent);
    border: 1px solid var(--color-dominant);
    cursor: pointer;
    pointer-events: auto;
  }

  .handle::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 2px;
    background: var(--color-accent);
    border: 1px solid var(--color-dominant);
    cursor: pointer;
    pointer-events: auto;
  }

  .handle:focus-visible {
    outline: none;
  }

  .handle:focus-visible::-webkit-slider-thumb {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .handle:focus-visible::-moz-range-thumb {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  .trim-readout {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-sm);
  }

  .trim-time {
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    font-size: var(--text-label);
    color: var(--color-body-text);
    white-space: nowrap;
  }
</style>
