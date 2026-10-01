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
</div>

<style>
  .presenter-select {
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

  .presenter-dropdown {
    padding: var(--space-sm);
    border: 1px solid var(--color-dominant);
    border-radius: var(--space-xs);
    background: var(--color-secondary);
    color: var(--color-body-text);
    font-family: var(--font-ui);
    font-size: var(--text-body);
    font-weight: var(--weight-regular);
    cursor: pointer;
    outline: none;
  }

  .presenter-dropdown:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .presenter-dropdown:focus-visible {
    box-shadow: 0 0 0 2px var(--color-accent);
  }
</style>
