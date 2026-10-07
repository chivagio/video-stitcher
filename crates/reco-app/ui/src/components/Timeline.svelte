<!--
  Timeline scrubber (UI-SPEC Component Inventory).
  <input type="range"> styled as a scrubber; step = 1 frame; frame-aligned.
  Disabled when no session (empty/loading states).
-->
<script lang="ts">
  import TrimHandles from "./TrimHandles.svelte";

  let {
    frame,
    total,
    disabled = false,
    onSeek,
    inFrame = null,
    outFrame = null,
    formatTimecode,
    onTrimChange,
  }: {
    frame: number;
    total: number | null;
    disabled?: boolean;
    onSeek: (frame: number) => void;
    /** The trim in point, or `null` for the clip start (EXPT-03). */
    inFrame?: number | null;
    /** The trim out point, or `null` for the clip end (EXPT-03). */
    outFrame?: number | null;
    /** Format a frame index as a timecode (the transport's exact formatter). */
    formatTimecode?: (frame: number) => string;
    /** Emit a trim change; `null` on a side means "the clip edge". */
    onTrimChange?: (inFrame: number | null, outFrame: number | null) => void;
  } = $props();

  let dragging = $state(false);
  let dragFrame = $state(0);
  let el = $state<HTMLInputElement | null>(null);

  function handleInput(e: Event): void {
    const value = Number((e.target as HTMLInputElement).value);
    dragFrame = value;
  }

  function handleChange(e: Event): void {
    const value = Number((e.target as HTMLInputElement).value);
    dragging = false;
    onSeek(value);
  }

  function handleKeyDown(e: KeyboardEvent): void {
    // Shift+Arrow is the pose nudge (App.svelte routes it to pose), so the
    // scrubber must not also seek a frame on the same keypress.
    if (e.shiftKey) return;
    if (e.key === "ArrowLeft") {
      e.preventDefault();
      onSeek(Math.max(0, frame - 1));
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      onSeek(Math.min((total ?? 1) - 1, frame + 1));
    }
  }

  $effect(() => {
    if (!dragging) {
      dragFrame = frame;
    }
  });

  $effect(() => {
    const input = el;
    if (!input) return;
    const pointerDown = () => { dragging = true; };
    const pointerUp = () => { dragging = false; };
    input.addEventListener("pointerdown", pointerDown);
    input.addEventListener("pointerup", pointerUp);
    return () => {
      input.removeEventListener("pointerdown", pointerDown);
      input.removeEventListener("pointerup", pointerUp);
    };
  });

  // `$derived`, not `const`: Svelte 5 evaluates the component body once, so a
  // plain `const` would freeze at mount — with `total` null that pinned the
  // range to `min=0 max=0 value=0`, so the playhead never advanced and a drag
  // could never seek (PREV-02).
  const max = $derived(total !== null && total > 0 ? total - 1 : 0);
  const value = $derived(dragging ? dragFrame : frame);

  // E3 "accent progress fill": the played fraction of the track, as a percentage
  // for the CSS gradient stop. Guarded against a zero-length track (no session
  // yet) so the gradient stays 0% instead of dividing by zero to NaN.
  const progress = $derived(max > 0 ? Math.min(100, (value / max) * 100) : 0);

  // The trim readout's frame→timecode formatter; the transport's exact
  // formatter is passed in, with a plain frame-index fallback for a caller that
  // has no rate yet.
  function defaultFormat(frame: number): string {
    return `${frame}`;
  }
</script>

<div class="timeline-stack">
  <input
    type="range"
    class="timeline"
    min="0"
    {max}
    step="1"
    {value}
    {disabled}
    style:--progress="{progress}%"
    aria-label="Timeline"
    aria-valuetext="{frame} of {total ?? 0}"
    oninput={handleInput}
    onchange={handleChange}
    onkeydown={handleKeyDown}
    bind:this={el}
  />

  {#if total !== null && total > 1 && onTrimChange}
    <TrimHandles
      {total}
      {inFrame}
      {outFrame}
      format={formatTimecode ?? defaultFormat}
      onChange={onTrimChange}
    />
  {/if}
</div>

<style>
  .timeline-stack {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: var(--space-xs);
    min-width: 0;
  }

  .timeline {
    flex: none;
    width: 100%;
    height: 4px;
    -webkit-appearance: none;
    appearance: none;
    background: var(--color-dominant);
    border-radius: 2px;
    cursor: pointer;
    outline: none;
  }

  .timeline:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .timeline::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    cursor: pointer;
  }

  .timeline::-moz-range-thumb {
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--color-accent);
    border: none;
    cursor: pointer;
  }

  .timeline:focus-visible {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  /* E3 accent progress fill. The thumb moves on its own, but the played part
     of the track is only visible if the runnable track is styled, so both
     engines get a gradient stopped at the reactive --progress custom property
     set inline by the component. */
  .timeline::-webkit-slider-runnable-track {
    height: 4px;
    border-radius: 2px;
    background: linear-gradient(
      to right,
      var(--color-accent) 0 var(--progress),
      var(--color-dominant) var(--progress) 100%
    );
  }

  .timeline::-moz-range-track {
    height: 4px;
    border-radius: 2px;
    background: linear-gradient(
      to right,
      var(--color-accent) 0 var(--progress),
      var(--color-dominant) var(--progress) 100%
    );
  }
</style>
