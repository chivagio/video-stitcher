<!--
  Timeline scrubber (UI-SPEC Component Inventory).
  <input type="range"> styled as a scrubber; step = 1 frame; frame-aligned.
  Disabled when no session (empty/loading states).
-->
<script lang="ts">
  let {
    frame,
    total,
    disabled = false,
    onSeek,
  }: {
    frame: number;
    total: number | null;
    disabled?: boolean;
    onSeek: (frame: number) => void;
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
</script>

<input
  type="range"
  class="timeline"
  min="0"
  {max}
  step="1"
  {value}
  {disabled}
  aria-label="Timeline"
  aria-valuetext="{frame} of {total ?? 0}"
  oninput={handleInput}
  onchange={handleChange}
  onkeydown={handleKeyDown}
  bind:this={el}
/>

<style>
  .timeline {
    flex: 1;
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
</style>
