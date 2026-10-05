<!--
  Presenter override select (UI-SPEC Component Inventory).
  Override menu: Auto / Native / Separate window / Readback.
  Auto is the default and reflects the probe result.
-->
<script lang="ts">
  import type { PresenterKind } from "../lib/types";

  let {
    value,
    disabled = false,
    onChange,
  }: {
    value: PresenterKind | "auto";
    disabled?: boolean;
    onChange: (value: PresenterKind | "auto") => void;
  } = $props();

  const options: { value: PresenterKind | "auto"; label: string }[] = [
    { value: "auto", label: "Auto" },
    { value: "native", label: "Native" },
    { value: "separate_window", label: "Separate window" },
    { value: "readback", label: "Readback" },
  ];

  function handleChange(e: Event): void {
    const v = (e.target as HTMLSelectElement).value as PresenterKind | "auto";
    onChange(v);
  }
</script>

<div class="presenter-select">
  <label for="presenter-select" class="presenter-label">Presenter</label>
  <div class="presenter-field">
    <select
      id="presenter-select"
      class="presenter-dropdown"
      {value}
      {disabled}
      aria-label="Presenter override"
      onchange={handleChange}
    >
      {#each options as opt}
        <option value={opt.value}>{opt.label}</option>
      {/each}
    </select>
    <!-- Hand-authored caret: `appearance: none` drops the GTK disclosure arrow,
         so the control needs its own to still read as a dropdown. UI-SPEC
         convention: inline SVG, stroke="currentColor", 16px. -->
    <span class="presenter-caret" aria-hidden="true">
      <svg
        width="16"
        height="16"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
        stroke-linecap="round"
        stroke-linejoin="round"
      >
        <polyline points="6 9 12 15 18 9" />
      </svg>
    </span>
  </div>
</div>

<style>
  .presenter-select {
    position: relative;
    display: flex;
    flex-direction: column;
    gap: var(--space-xs);
  }

  .presenter-label {
    font-family: var(--font-ui);
    font-size: var(--text-label);
    font-weight: var(--weight-semibold);
    line-height: var(--line-tight);
    color: var(--color-body-text);
  }

  /* Wraps the select + caret so the caret positions against the control row,
     not the label + control column. */
  .presenter-field {
    position: relative;
    display: flex;
  }

  .presenter-dropdown {
    flex: 1;
    padding: var(--space-sm);
    /* Leave room for the 16px caret at the right edge plus a gap, so the
       selected value never sits under it. */
    padding-right: calc(var(--space-sm) + 16px + var(--space-xs));
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    /* Drop the native widget appearance: under WebKitGTK an unstyled select is
       painted by the GTK menulist (near-white), which bypasses the authored
       background below and leaves #e8e8e8 text at ~1.2:1. Mirrors FovSlider. */
    -webkit-appearance: none;
    appearance: none;
    /* background: shorthand first, then explicitly clear any UA gradient/image
       so nothing paints behind the authored colour. */
    background: var(--color-secondary);
    background-image: none;
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    cursor: pointer;
    outline: none;
  }

  .presenter-caret {
    position: absolute;
    right: var(--space-sm);
    top: 50%;
    transform: translateY(-50%);
    display: flex;
    align-items: center;
    color: var(--color-body-text);
    /* Never intercept the click that opens the menu. */
    pointer-events: none;
  }

  .presenter-dropdown:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* Match the select's own disabled opacity so the caret dims with it. */
  .presenter-select:has(.presenter-dropdown:disabled) .presenter-caret {
    opacity: 0.5;
  }

  .presenter-dropdown:focus-visible {
    box-shadow: 0 0 0 2px var(--color-accent);
  }
</style>
